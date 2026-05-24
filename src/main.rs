use libc::{
    CMSG_DATA, CMSG_FIRSTHDR, CMSG_SPACE, MSG_CMSG_CLOEXEC, POLLIN, SCM_RIGHTS, SOL_SOCKET, c_void,
    cmsghdr, iovec, msghdr, poll, pollfd, recvmsg,
};
use serde::Deserialize;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::mem;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;
use std::thread;

const DIMENSIONS: usize = 14;
const BLOCK_LANES: usize = 8;
const QUANT_SCALE: f32 = 10_000.0;
const MAX_PENDING: usize = 16 * 1024;

const READY_RESPONSE: &[u8] = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
const NOT_FOUND_RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
const BAD_REQUEST_RESPONSE: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
const FRAUD_RESPONSES: [&[u8]; 6] = [
    b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.0}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}",
    b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":1.0}",
];

#[derive(Clone)]
struct IvfConfig {
    fast_nprobe: u32,
    full_nprobe: u32,
    boundary_full: bool,
    bbox_repair: bool,
    repair_min: u8,
    repair_max: u8,
}

impl IvfConfig {
    fn from_env() -> Self {
        let fast_nprobe = env_u32("IVF_FAST_NPROBE", 1);
        let full_nprobe = env_u32("IVF_FULL_NPROBE", fast_nprobe);
        Self {
            fast_nprobe,
            full_nprobe,
            boundary_full: env_bool("IVF_BOUNDARY_FULL", full_nprobe > fast_nprobe),
            bbox_repair: env_bool("IVF_BBOX_REPAIR", true),
            repair_min: env_u32("IVF_REPAIR_MIN_FRAUDS", 2).min(5) as u8,
            repair_max: env_u32("IVF_REPAIR_MAX_FRAUDS", 3).min(5) as u8,
        }
    }
}

struct IvfIndex {
    n: u32,
    clusters: u32,
    centroids: Vec<f32>,
    bbox_min: Vec<i16>,
    bbox_max: Vec<i16>,
    offsets: Vec<u32>,
    labels: Vec<u8>,
    orig_ids: Vec<u32>,
    blocks: Vec<i16>,
}

impl IvfIndex {
    fn load(path: &str) -> Result<Self, String> {
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|err| format!("falha ao abrir indice IVF {path}: {err}"))?
            .read_to_end(&mut bytes)
            .map_err(|err| format!("falha ao ler indice IVF {path}: {err}"))?;
        let mut cursor = 0usize;
        let magic = read_bytes::<4>(&bytes, &mut cursor)?;
        if &magic != b"IVF8" {
            return Err("indice IVF com magic invalido".into());
        }
        let n = read_u32(&bytes, &mut cursor)?;
        let clusters = read_u32(&bytes, &mut cursor)?;
        let dim = read_u32(&bytes, &mut cursor)?;
        let scale = read_u32(&bytes, &mut cursor)?;
        let total_blocks = read_u32(&bytes, &mut cursor)?;
        if dim != DIMENSIONS as u32 || scale != QUANT_SCALE as u32 || clusters == 0 {
            return Err("indice IVF incompativel".into());
        }
        let padded_rows = total_blocks as usize * BLOCK_LANES;
        let centroids = read_f32_vec(&bytes, &mut cursor, clusters as usize * DIMENSIONS)?;
        let bbox_min = read_i16_vec(&bytes, &mut cursor, clusters as usize * DIMENSIONS)?;
        let bbox_max = read_i16_vec(&bytes, &mut cursor, clusters as usize * DIMENSIONS)?;
        let offsets = read_u32_vec(&bytes, &mut cursor, clusters as usize + 1)?;
        let labels = read_u8_vec(&bytes, &mut cursor, padded_rows)?;
        let orig_ids = read_u32_vec(&bytes, &mut cursor, padded_rows)?;
        let blocks = read_i16_vec(
            &bytes,
            &mut cursor,
            total_blocks as usize * DIMENSIONS * BLOCK_LANES,
        )?;
        if offsets.first() != Some(&0) || offsets.last() != Some(&total_blocks) {
            return Err("offsets invalidos no indice IVF".into());
        }
        Ok(Self {
            n,
            clusters,
            centroids,
            bbox_min,
            bbox_max,
            offsets,
            labels,
            orig_ids,
            blocks,
        })
    }

    fn fraud_count(&self, query: &[f32; DIMENSIONS], config: &IvfConfig) -> u8 {
        let mut query_i16 = [0i16; DIMENSIONS];
        for (index, value) in query.iter().enumerate() {
            query_i16[index] = quantize(*value);
        }
        let fast_nprobe = config.fast_nprobe.max(1).min(self.clusters);
        let fast_repair = config.bbox_repair && !config.boundary_full;
        let mut frauds = self.fraud_count_once(&query_i16, query, fast_nprobe, fast_repair);
        if config.boundary_full
            && ((frauds >= config.repair_min && frauds <= config.repair_max)
                || should_repair_extreme(frauds, query))
        {
            let full_nprobe = config.full_nprobe.max(fast_nprobe).min(self.clusters);
            frauds = self.fraud_count_once(&query_i16, query, full_nprobe, config.bbox_repair);
        }
        frauds
    }

    fn fraud_count_once(
        &self,
        query_i16: &[i16; DIMENSIONS],
        query: &[f32; DIMENSIONS],
        nprobe: u32,
        repair: bool,
    ) -> u8 {
        if self.n < 5 || self.clusters == 0 {
            return 0;
        }
        let mut best_clusters = vec![0u32; nprobe as usize];
        let mut best_distances = vec![f32::INFINITY; nprobe as usize];
        if nprobe == 1 {
            best_clusters[0] = self.nearest_centroid(query);
        } else {
            for cluster in 0..self.clusters {
                let mut distance = 0.0f32;
                for dim in 0..DIMENSIONS {
                    let centroid =
                        self.centroids[(dim * self.clusters as usize) + cluster as usize];
                    let delta = query[dim] - centroid;
                    distance += delta * delta;
                }
                insert_probe(cluster, distance, &mut best_clusters, &mut best_distances);
            }
        }

        let mut top = Top5::new();
        for cluster in best_clusters.iter().copied() {
            self.scan_blocks(
                &mut top,
                self.offsets[cluster as usize],
                self.offsets[cluster as usize + 1],
                query_i16,
            );
        }

        if repair {
            for cluster in 0..self.clusters {
                if self.offsets[cluster as usize] == self.offsets[cluster as usize + 1] {
                    continue;
                }
                if best_clusters.contains(&cluster) {
                    continue;
                }
                let worst = top.worst_distance();
                if self.bbox_lower_bound(cluster, query_i16, worst) <= worst {
                    self.scan_blocks(
                        &mut top,
                        self.offsets[cluster as usize],
                        self.offsets[cluster as usize + 1],
                        query_i16,
                    );
                }
            }
        }
        top.frauds()
    }

    fn nearest_centroid(&self, query: &[f32; DIMENSIONS]) -> u32 {
        let mut best_cluster = 0u32;
        let mut best_distance = f32::INFINITY;
        for cluster in 0..self.clusters {
            let mut distance = 0.0f32;
            for dim in 0..DIMENSIONS {
                let centroid = self.centroids[(dim * self.clusters as usize) + cluster as usize];
                let delta = query[dim] - centroid;
                distance += delta * delta;
            }
            if distance < best_distance {
                best_distance = distance;
                best_cluster = cluster;
            }
        }
        best_cluster
    }

    fn bbox_lower_bound(&self, cluster: u32, query: &[i16; DIMENSIONS], stop_after: u64) -> u64 {
        let base = cluster as usize * DIMENSIONS;
        let mut sum = 0u64;
        for dim in 0..DIMENSIONS {
            let target = query[dim];
            if target < self.bbox_min[base + dim] {
                sum += sqdiff_i16(target, self.bbox_min[base + dim]);
            } else if target > self.bbox_max[base + dim] {
                sum += sqdiff_i16(target, self.bbox_max[base + dim]);
            }
            if sum > stop_after {
                return sum;
            }
        }
        sum
    }

    fn scan_blocks(
        &self,
        top: &mut Top5,
        start_block: u32,
        end_block: u32,
        query: &[i16; DIMENSIONS],
    ) {
        for block in start_block..end_block {
            let block_base = block as usize * DIMENSIONS * BLOCK_LANES;
            let label_base = block as usize * BLOCK_LANES;
            for lane in 0..BLOCK_LANES {
                let id = self.orig_ids[label_base + lane];
                if id == u32::MAX {
                    continue;
                }
                let mut distance = 0u64;
                for dim in 0..DIMENSIONS {
                    distance += sqdiff_i16(
                        query[dim],
                        self.blocks[block_base + (dim * BLOCK_LANES) + lane],
                    );
                    if distance > top.worst_distance() {
                        break;
                    }
                }
                top.insert(distance, self.labels[label_base + lane], id);
            }
        }
    }
}

struct Top5 {
    distances: [u64; 5],
    labels: [u8; 5],
    ids: [u32; 5],
    worst: usize,
}

impl Top5 {
    fn new() -> Self {
        Self {
            distances: [u64::MAX; 5],
            labels: [0; 5],
            ids: [u32::MAX; 5],
            worst: 0,
        }
    }

    fn better(&self, distance: u64, id: u32, pos: usize) -> bool {
        distance < self.distances[pos] || (distance == self.distances[pos] && id < self.ids[pos])
    }

    fn refresh_worst(&mut self) {
        self.worst = 0;
        for pos in 1..self.distances.len() {
            if self.distances[pos] > self.distances[self.worst]
                || (self.distances[pos] == self.distances[self.worst]
                    && self.ids[pos] > self.ids[self.worst])
            {
                self.worst = pos;
            }
        }
    }

    fn insert(&mut self, distance: u64, label: u8, id: u32) {
        if !self.better(distance, id, self.worst) {
            return;
        }
        self.distances[self.worst] = distance;
        self.labels[self.worst] = label;
        self.ids[self.worst] = id;
        self.refresh_worst();
    }

    fn worst_distance(&self) -> u64 {
        self.distances[self.worst]
    }

    fn frauds(&self) -> u8 {
        self.labels.iter().filter(|&&label| label != 0).count() as u8
    }
}

#[derive(Deserialize)]
struct Payload {
    transaction: Transaction,
    customer: Customer,
    merchant: Merchant,
    terminal: Terminal,
    last_transaction: Option<LastTransaction>,
}

#[derive(Deserialize)]
struct Transaction {
    amount: f32,
    installments: u32,
    requested_at: String,
}

#[derive(Deserialize)]
struct Customer {
    avg_amount: f32,
    tx_count_24h: u32,
    known_merchants: Vec<String>,
}

#[derive(Deserialize)]
struct Merchant {
    id: String,
    mcc: String,
    avg_amount: f32,
}

#[derive(Deserialize)]
struct Terminal {
    is_online: bool,
    card_present: bool,
    km_from_home: f32,
}

#[derive(Deserialize)]
struct LastTransaction {
    timestamp: String,
    km_from_current: f32,
}

#[derive(Clone, Copy)]
struct Timestamp {
    total_seconds: i64,
    hour: u8,
    weekday_monday0: u8,
}

fn vectorize(body: &[u8]) -> Option<[f32; DIMENSIONS]> {
    vectorize_manual(body).or_else(|| vectorize_serde(body))
}

fn vectorize_serde(body: &[u8]) -> Option<[f32; DIMENSIONS]> {
    let payload: Payload = serde_json::from_slice(body).ok()?;
    let requested = parse_timestamp(&payload.transaction.requested_at)?;
    let mut minutes_since_last = -1.0f32;
    let mut km_from_last = -1.0f32;
    if let Some(last) = payload.last_transaction {
        let last_ts = parse_timestamp(&last.timestamp)?;
        let elapsed = ((requested.total_seconds - last_ts.total_seconds).max(0) / 60) as f32;
        minutes_since_last = clamp01(elapsed / 1440.0);
        km_from_last = clamp01(last.km_from_current / 1000.0);
    }
    let known_merchant = payload
        .customer
        .known_merchants
        .iter()
        .any(|merchant| merchant == &payload.merchant.id);
    let amount_vs_avg = if payload.customer.avg_amount <= 0.0 {
        if payload.transaction.amount <= 0.0 {
            0.0
        } else {
            1.0
        }
    } else {
        (payload.transaction.amount / payload.customer.avg_amount) / 10.0
    };
    Some([
        clamp01(payload.transaction.amount / 10000.0),
        clamp01(payload.transaction.installments as f32 / 12.0),
        clamp01(amount_vs_avg),
        requested.hour as f32 / 23.0,
        requested.weekday_monday0 as f32 / 6.0,
        minutes_since_last,
        km_from_last,
        clamp01(payload.terminal.km_from_home / 1000.0),
        clamp01(payload.customer.tx_count_24h as f32 / 20.0),
        if payload.terminal.is_online { 1.0 } else { 0.0 },
        if payload.terminal.card_present {
            1.0
        } else {
            0.0
        },
        if known_merchant { 0.0 } else { 1.0 },
        mcc_risk(&payload.merchant.mcc),
        clamp01(payload.merchant.avg_amount / 10000.0),
    ])
}

fn vectorize_manual(body: &[u8]) -> Option<[f32; DIMENSIONS]> {
    let mut cursor = 0usize;

    cursor = next_value(body, cursor)?;
    scan_json_string(body, &mut cursor)?;

    cursor = next_value(body, cursor)?;
    skip_object_start(body, &mut cursor);
    cursor = next_value(body, cursor)?;
    let amount = scan_f32(body, &mut cursor)?;
    cursor = next_value(body, cursor)?;
    let installments = scan_u32(body, &mut cursor)?;
    cursor = next_value(body, cursor)?;
    let requested = scan_timestamp(body, &mut cursor)?;

    cursor = next_value(body, cursor)?;
    skip_object_start(body, &mut cursor);
    cursor = next_value(body, cursor)?;
    let customer_avg_amount = scan_f32(body, &mut cursor)?;
    cursor = next_value(body, cursor)?;
    let tx_count_24h = scan_u32(body, &mut cursor)?;
    cursor = next_value(body, cursor)?;
    let known_merchants = scan_known_merchants(body, &mut cursor)?;

    cursor = next_value(body, cursor)?;
    skip_object_start(body, &mut cursor);
    cursor = next_value(body, cursor)?;
    let merchant_id = scan_merchant_code(body, &mut cursor)?;
    cursor = next_value(body, cursor)?;
    let mcc_risk = scan_mcc_risk(body, &mut cursor)?;
    cursor = next_value(body, cursor)?;
    let merchant_avg_amount = scan_f32(body, &mut cursor)?;

    cursor = next_value(body, cursor)?;
    skip_object_start(body, &mut cursor);
    cursor = next_value(body, cursor)?;
    let is_online = scan_bool(body, &mut cursor)?;
    cursor = next_value(body, cursor)?;
    let card_present = scan_bool(body, &mut cursor)?;
    cursor = next_value(body, cursor)?;
    let km_from_home = scan_f32(body, &mut cursor)?;

    cursor = next_value(body, cursor)?;
    let mut minutes_since_last = -1.0f32;
    let mut km_from_last = -1.0f32;
    cursor = skip_space(body, cursor);
    if body.get(cursor..cursor + 4) == Some(b"null") {
    } else {
        skip_object_start(body, &mut cursor);
        cursor = next_value(body, cursor)?;
        let last_ts = scan_timestamp(body, &mut cursor)?;
        cursor = next_value(body, cursor)?;
        let last_km = scan_f32(body, &mut cursor)?;
        let elapsed = ((requested.total_seconds - last_ts.total_seconds).max(0) / 60) as f32;
        minutes_since_last = clamp01(elapsed / 1440.0);
        km_from_last = clamp01(last_km / 1000.0);
    }

    let known_merchant = known_merchants.contains(&merchant_id);
    let amount_vs_avg = if customer_avg_amount <= 0.0 {
        if amount <= 0.0 { 0.0 } else { 1.0 }
    } else {
        (amount / customer_avg_amount) / 10.0
    };

    Some([
        clamp01(amount / 10000.0),
        clamp01(installments as f32 / 12.0),
        clamp01(amount_vs_avg),
        requested.hour as f32 / 23.0,
        requested.weekday_monday0 as f32 / 6.0,
        minutes_since_last,
        km_from_last,
        clamp01(km_from_home / 1000.0),
        clamp01(tx_count_24h as f32 / 20.0),
        if is_online { 1.0 } else { 0.0 },
        if card_present { 1.0 } else { 0.0 },
        if known_merchant { 0.0 } else { 1.0 },
        mcc_risk,
        clamp01(merchant_avg_amount / 10000.0),
    ])
}

fn next_value(body: &[u8], cursor: usize) -> Option<usize> {
    let colon = body[cursor..].iter().position(|&byte| byte == b':')?;
    Some(skip_space(body, cursor + colon + 1))
}

fn skip_space(body: &[u8], mut cursor: usize) -> usize {
    while matches!(body.get(cursor), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        cursor += 1;
    }
    cursor
}

fn skip_object_start(body: &[u8], cursor: &mut usize) {
    *cursor = skip_space(body, *cursor);
    if body.get(*cursor) == Some(&b'{') {
        *cursor += 1;
    }
}

fn scan_json_string<'a>(body: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    *cursor = skip_space(body, *cursor);
    if body.get(*cursor) != Some(&b'"') {
        return None;
    }
    *cursor += 1;
    let start = *cursor;
    while let Some(&byte) = body.get(*cursor) {
        if byte == b'\\' {
            return None;
        }
        if byte == b'"' {
            let out = &body[start..*cursor];
            *cursor += 1;
            return Some(out);
        }
        *cursor += 1;
    }
    None
}

fn scan_f32(body: &[u8], cursor: &mut usize) -> Option<f32> {
    *cursor = skip_space(body, *cursor);
    let mut sign = 1.0f64;
    if body.get(*cursor) == Some(&b'-') {
        sign = -1.0;
        *cursor += 1;
    }
    let mut seen = false;
    let mut value = 0f64;
    while let Some(byte) = body.get(*cursor).copied() {
        if !byte.is_ascii_digit() {
            break;
        }
        seen = true;
        value = value * 10.0 + f64::from(byte - b'0');
        *cursor += 1;
    }
    if body.get(*cursor) == Some(&b'.') {
        *cursor += 1;
        let mut scale = 0.1f64;
        while let Some(byte) = body.get(*cursor).copied() {
            if !byte.is_ascii_digit() {
                break;
            }
            seen = true;
            value += f64::from(byte - b'0') * scale;
            scale *= 0.1;
            *cursor += 1;
        }
    }
    if matches!(body.get(*cursor), Some(b'e' | b'E')) {
        return None;
    }
    seen.then_some((value * sign) as f32)
}

fn scan_u32(body: &[u8], cursor: &mut usize) -> Option<u32> {
    *cursor = skip_space(body, *cursor);
    let mut seen = false;
    let mut value = 0u32;
    while let Some(byte) = body.get(*cursor).copied() {
        if !byte.is_ascii_digit() {
            break;
        }
        seen = true;
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
        *cursor += 1;
    }
    seen.then_some(value)
}

fn scan_bool(body: &[u8], cursor: &mut usize) -> Option<bool> {
    *cursor = skip_space(body, *cursor);
    if body.get(*cursor..*cursor + 4) == Some(b"true") {
        *cursor += 4;
        Some(true)
    } else if body.get(*cursor..*cursor + 5) == Some(b"false") {
        *cursor += 5;
        Some(false)
    } else {
        None
    }
}

fn scan_known_merchants(body: &[u8], cursor: &mut usize) -> Option<[u16; 16]> {
    *cursor = skip_space(body, *cursor);
    if body.get(*cursor) != Some(&b'[') {
        return None;
    }
    *cursor += 1;
    let mut out = [u16::MAX; 16];
    let mut len = 0usize;
    loop {
        *cursor = skip_space(body, *cursor);
        if body.get(*cursor) == Some(&b']') {
            *cursor += 1;
            return Some(out);
        }
        let code = scan_merchant_code(body, cursor)?;
        if len < out.len() {
            out[len] = code;
            len += 1;
        }
        *cursor = skip_space(body, *cursor);
        match body.get(*cursor) {
            Some(b',') => *cursor += 1,
            Some(b']') => {
                *cursor += 1;
                return Some(out);
            }
            _ => return None,
        }
    }
}

fn scan_merchant_code(body: &[u8], cursor: &mut usize) -> Option<u16> {
    let value = scan_json_string(body, cursor)?;
    if value.len() != 8 || &value[..5] != b"MERC-" {
        return None;
    }
    let hundreds = value[5].checked_sub(b'0')?;
    let tens = value[6].checked_sub(b'0')?;
    let ones = value[7].checked_sub(b'0')?;
    if hundreds > 9 || tens > 9 || ones > 9 {
        return None;
    }
    Some(u16::from(hundreds) * 100 + u16::from(tens) * 10 + u16::from(ones))
}

fn scan_mcc_risk(body: &[u8], cursor: &mut usize) -> Option<f32> {
    let value = scan_json_string(body, cursor)?;
    Some(match value {
        b"5411" => 0.15,
        b"5812" => 0.30,
        b"5912" => 0.20,
        b"5944" => 0.45,
        b"7801" => 0.80,
        b"7802" => 0.75,
        b"7995" => 0.85,
        b"4511" => 0.35,
        b"5311" => 0.25,
        b"5999" => 0.50,
        _ => 0.50,
    })
}

fn scan_timestamp(body: &[u8], cursor: &mut usize) -> Option<Timestamp> {
    let value = scan_json_string(body, cursor)?;
    if value.len() != 20 {
        return None;
    }
    let year = digits(&value[0..4])? as i32;
    let month = digits(&value[5..7])? as u8;
    let day = digits(&value[8..10])? as u8;
    let hour = digits(&value[11..13])? as u8;
    let minute = digits(&value[14..16])? as u8;
    let second = digits(&value[17..19])? as u8;
    let days = days_from_civil(year, month, day);
    Some(Timestamp {
        total_seconds: days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64,
        hour,
        weekday_monday0: ((days + 3) % 7) as u8,
    })
}

fn main() {
    let index_path =
        env::var("IVF_INDEX_PATH").unwrap_or_else(|_| "/app/data/index.bin".to_string());
    let socket_path = env::var("UNIX_SOCKET_PATH").expect("UNIX_SOCKET_PATH obrigatorio");
    let index = Arc::new(IvfIndex::load(&index_path).expect("indice IVF valido"));
    let config = Arc::new(IvfConfig::from_env());
    run_server(&socket_path, index, config).expect("servidor unix funcional");
}

fn run_server(
    socket_path: &str,
    index: Arc<IvfIndex>,
    config: Arc<IvfConfig>,
) -> std::io::Result<()> {
    let path = Path::new(socket_path);
    if path.exists() {
        fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o777))?;

    {
        let ctrl_path = format!("{socket_path}.ctrl");
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        thread::spawn(move || {
            let _ = run_control_socket(&ctrl_path, index, config);
        });
    }

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let index = Arc::clone(&index);
                let config = Arc::clone(&config);
                thread::spawn(move || {
                    let _ = handle_stream(stream, index, config);
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                let mut fds = [pollfd {
                    fd: listener.as_raw_fd(),
                    events: POLLIN,
                    revents: 0,
                }];
                unsafe {
                    poll(fds.as_mut_ptr(), 1, 100);
                }
            }
            Err(err) => return Err(err),
        }
    }
}

fn run_control_socket(
    ctrl_path: &str,
    index: Arc<IvfIndex>,
    config: Arc<IvfConfig>,
) -> std::io::Result<()> {
    let path = Path::new(ctrl_path);
    if path.exists() {
        fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o777))?;
    loop {
        let (conn, _) = listener.accept()?;
        loop {
            let Some(fd) = receive_fd(conn.as_raw_fd()) else {
                break;
            };
            let index = Arc::clone(&index);
            let config = Arc::clone(&config);
            thread::spawn(move || {
                let stream = unsafe { TcpStream::from_raw_fd(fd) };
                let _ = stream.set_nodelay(true);
                let _ = handle_stream(stream, index, config);
            });
        }
    }
}

fn handle_stream<S: Read + Write>(
    mut stream: S,
    index: Arc<IvfIndex>,
    config: Arc<IvfConfig>,
) -> std::io::Result<()> {
    let mut input = Vec::with_capacity(MAX_PENDING);
    let mut buffer = [0u8; 4096];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        input.extend_from_slice(&buffer[..read]);
        if input.len() > MAX_PENDING {
            return Ok(());
        }
        while let Some(response) = process_one(&mut input, &index, &config) {
            stream.write_all(response.as_slice())?;
        }
    }
}

fn receive_fd(socket_fd: RawFd) -> Option<RawFd> {
    let mut byte = 0u8;
    let mut iov = iovec {
        iov_base: (&mut byte as *mut u8).cast::<c_void>(),
        iov_len: mem::size_of::<u8>(),
    };
    let mut control = vec![0u8; unsafe { CMSG_SPACE(mem::size_of::<RawFd>() as u32) as usize }];
    let mut message: msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast::<c_void>();
    message.msg_controllen = control.len();

    let received = unsafe { recvmsg(socket_fd, &mut message, MSG_CMSG_CLOEXEC) };
    if received <= 0 {
        return None;
    }

    let cmsg = unsafe { CMSG_FIRSTHDR(&message) };
    if cmsg.is_null() {
        return None;
    }
    let header: &cmsghdr = unsafe { &*cmsg };
    if header.cmsg_level != SOL_SOCKET || header.cmsg_type != SCM_RIGHTS {
        return None;
    }
    let data = unsafe { CMSG_DATA(cmsg).cast::<RawFd>() };
    let fd = unsafe { *data };
    (fd >= 0).then_some(fd)
}

fn process_one(input: &mut Vec<u8>, index: &IvfIndex, config: &IvfConfig) -> Option<Vec<u8>> {
    let header_end = find_header_end(input)?;
    let header_len = header_end + 4;
    let header = &input[..header_end];
    let first_line_end = header.iter().position(|&byte| byte == b'\n')?;
    let first_line = trim_cr(&header[..first_line_end]);
    let content_length = parse_content_length(&header[first_line_end + 1..]).unwrap_or(0);
    let total_len = header_len + content_length;
    if input.len() < total_len {
        return None;
    }
    let body = &input[header_len..total_len];
    let response = if first_line.starts_with(b"GET /ready ") {
        READY_RESPONSE.to_vec()
    } else if first_line.starts_with(b"POST /fraud-score ") {
        let fraud_count = vectorize(body)
            .map(|query| index.fraud_count(&query, config))
            .unwrap_or(0);
        FRAUD_RESPONSES[fraud_count.min(5) as usize].to_vec()
    } else if first_line.is_empty() {
        BAD_REQUEST_RESPONSE.to_vec()
    } else {
        NOT_FOUND_RESPONSE.to_vec()
    };
    input.drain(..total_len);
    Some(response)
}

fn find_header_end(input: &[u8]) -> Option<usize> {
    input.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    for line in headers.split(|&byte| byte == b'\n') {
        let line = trim_cr(line);
        let colon = line.iter().position(|&byte| byte == b':')?;
        if eq_ascii_case(&line[..colon], b"content-length") {
            let mut value = 0usize;
            let mut seen = false;
            for byte in trim_space(&line[colon + 1..]) {
                if !byte.is_ascii_digit() {
                    break;
                }
                seen = true;
                value = value * 10 + (byte - b'0') as usize;
            }
            if seen {
                return Some(value);
            }
        }
    }
    None
}

fn trim_cr(value: &[u8]) -> &[u8] {
    value.strip_suffix(b"\r").unwrap_or(value)
}

fn trim_space(mut value: &[u8]) -> &[u8] {
    while matches!(value.first(), Some(b' ' | b'\t')) {
        value = &value[1..];
    }
    while matches!(value.last(), Some(b' ' | b'\t' | b'\r')) {
        value = &value[..value.len() - 1];
    }
    value
}

fn eq_ascii_case(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn parse_timestamp(value: &str) -> Option<Timestamp> {
    let bytes = value.as_bytes();
    if bytes.len() != 20 {
        return None;
    }
    let year = digits(&bytes[0..4])? as i32;
    let month = digits(&bytes[5..7])? as u8;
    let day = digits(&bytes[8..10])? as u8;
    let hour = digits(&bytes[11..13])? as u8;
    let minute = digits(&bytes[14..16])? as u8;
    let second = digits(&bytes[17..19])? as u8;
    let days = days_from_civil(year, month, day);
    Some(Timestamp {
        total_seconds: days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64,
        hour,
        weekday_monday0: ((days + 3) % 7) as u8,
    })
}

fn digits(bytes: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (byte - b'0') as u32;
    }
    Some(value)
}

fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let adjusted_year = year - i32::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month as i32 + if month > 2 { -3 } else { 9 };
    let day_of_year = ((153 * shifted_month) + 2) / 5 + day as i32 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * 146_097 + day_of_era - 719_468) as i64
}

fn mcc_risk(mcc: &str) -> f32 {
    match mcc {
        "5411" => 0.15,
        "5812" => 0.30,
        "5912" => 0.20,
        "5944" => 0.45,
        "7801" => 0.80,
        "7802" => 0.75,
        "7995" => 0.85,
        "4511" => 0.35,
        "5311" => 0.25,
        "5999" => 0.50,
        _ => 0.50,
    }
}

fn should_repair_extreme(frauds: u8, query: &[f32; DIMENSIONS]) -> bool {
    let old_last_transaction = query[5] >= 0.99;
    if old_last_transaction {
        if frauds == 0 {
            return query[0] >= 0.08
                && query[0] <= 0.29
                && query[1] >= 0.25
                && query[1] <= 0.59
                && query[2] >= 0.40
                && query[6] >= 0.03
                && query[6] <= 0.29
                && query[7] >= 0.03
                && query[7] <= 0.41
                && query[8] <= 0.56
                && query[13] >= 0.008
                && query[13] <= 0.026;
        }

        if frauds == 5 {
            return query[0] >= 0.07
                && query[0] <= 0.48
                && query[1] >= 0.33
                && query[1] <= 0.76
                && query[2] >= 0.50
                && query[6] >= 0.07
                && query[6] <= 0.53
                && query[7] >= 0.06
                && query[7] <= 0.49
                && query[8] >= 0.40
                && query[8] <= 0.70
                && query[13] <= 0.028;
        }
    }

    let no_last_transaction = query[5] < -0.5 && query[6] < -0.5;
    if !no_last_transaction {
        return false;
    }
    if frauds == 0 {
        return query[0] >= 0.08
            && query[0] <= 0.13
            && query[2] >= 0.22
            && query[2] <= 0.38
            && query[7] >= 0.07
            && query[7] <= 0.13
            && query[8] >= 0.18
            && query[8] <= 0.22
            && query[9] < 0.5
            && query[11] < 0.5
            && query[12] >= 0.25
            && query[12] <= 0.50;
    }
    if frauds == 5 {
        return query[0] >= 0.24
            && query[0] <= 0.30
            && query[2] >= 0.82
            && query[2] <= 0.90
            && query[7] >= 0.35
            && query[7] <= 0.45
            && query[8] >= 0.45
            && query[8] <= 0.55
            && query[9] < 0.5
            && query[10] >= 0.5
            && query[11] >= 0.5
            && query[12] >= 0.75;
    }
    false
}

fn insert_probe(
    cluster: u32,
    distance: f32,
    best_clusters: &mut [u32],
    best_distances: &mut [f32],
) {
    let nprobe = best_distances.len();
    if distance >= best_distances[nprobe - 1] {
        return;
    }
    let mut pos = nprobe - 1;
    while pos > 0 && distance < best_distances[pos - 1] {
        best_distances[pos] = best_distances[pos - 1];
        best_clusters[pos] = best_clusters[pos - 1];
        pos -= 1;
    }
    best_distances[pos] = distance;
    best_clusters[pos] = cluster;
}

fn quantize(value: f32) -> i16 {
    let value = value.clamp(-1.0, 1.0);
    let rounded = (value * QUANT_SCALE).round() as i32;
    rounded.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn sqdiff_i16(left: i16, right: i16) -> u64 {
    let delta = left as i64 - right as i64;
    (delta * delta) as u64
}

fn clamp01(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

fn env_u32(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .map(|value| value == "1" || value == "true" || value == "TRUE")
        .unwrap_or(default)
}

fn read_bytes<const N: usize>(bytes: &[u8], cursor: &mut usize) -> Result<[u8; N], String> {
    if *cursor + N > bytes.len() {
        return Err("indice IVF truncado".into());
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes[*cursor..*cursor + N]);
    *cursor += N;
    Ok(out)
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(read_bytes::<4>(bytes, cursor)?))
}

fn read_f32_vec(bytes: &[u8], cursor: &mut usize, len: usize) -> Result<Vec<f32>, String> {
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(f32::from_le_bytes(read_bytes::<4>(bytes, cursor)?));
    }
    Ok(out)
}

fn read_i16_vec(bytes: &[u8], cursor: &mut usize, len: usize) -> Result<Vec<i16>, String> {
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(i16::from_le_bytes(read_bytes::<2>(bytes, cursor)?));
    }
    Ok(out)
}

fn read_u32_vec(bytes: &[u8], cursor: &mut usize, len: usize) -> Result<Vec<u32>, String> {
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(read_u32(bytes, cursor)?);
    }
    Ok(out)
}

fn read_u8_vec(bytes: &[u8], cursor: &mut usize, len: usize) -> Result<Vec<u8>, String> {
    if *cursor + len > bytes.len() {
        return Err("indice IVF truncado".into());
    }
    let out = bytes[*cursor..*cursor + len].to_vec();
    *cursor += len;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_vectorizer_matches_serde_for_compact_payload() {
        let payload = br#"{"id":"tx-3576980410","transaction":{"amount":384.88,"installments":3,"requested_at":"2026-03-11T20:23:35Z"},"customer":{"avg_amount":769.76,"tx_count_24h":3,"known_merchants":["MERC-009","MERC-001","MERC-001"]},"merchant":{"id":"MERC-001","mcc":"5912","avg_amount":298.95},"terminal":{"is_online":false,"card_present":true,"km_from_home":13.7090520965},"last_transaction":{"timestamp":"2026-03-11T14:58:35Z","km_from_current":18.8626479774}}"#;

        let manual = vectorize_manual(payload).expect("manual parser should parse compact official payload");
        let serde = vectorize_serde(payload).expect("serde parser should parse compact official payload");

        assert_eq!(manual, serde);
    }
}
