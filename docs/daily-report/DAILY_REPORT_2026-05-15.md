# Daily Report - 2026-05-15 - Rust IVF paralelo

## Objetivo

- Criar uma submissão paralela em nova linguagem sem alterar a submissão C++ atual `andrade-cpp-ivf`.
- Linguagem escolhida: Rust.
- Justificativa: Rust mantém controle fino de memória, sockets Unix, baixo overhead e caminho viável para SIMD/mmap, com iteração mais rápida que C/Zig para uma segunda stack.

## Registro oficial

- PR oficial mergeado: `https://github.com/zanfranceschi/rinha-de-backend-2026/pull/4636`.
- Novo ID: `andrade-rust-ivf`.
- Repo público: `https://github.com/viniciusdsandrade/rinha-de-backend-2026-rust`.
- O primeiro run do PR falhou porque o repo Rust tinha apenas uma branch.
- Correção aplicada:
  - `main` com código fonte Rust.
  - `submission` com somente `docker-compose.yml` e `info.json`.
  - Novo push no PR para reexecutar validação.
- O smoke test oficial do PR passou para C++ e Rust após novo push.
- A primeira issue oficial de teste da Rust (`#4638`) foi rejeitada por memória: `170MB + 170MB + 30MB = 370MB`.
- Correção aplicada na composição Rust: APIs em `160MB` cada e LB em `30MB`, totalizando o limite oficial de `350MB`.
- Nova issue oficial de teste: `https://github.com/zanfranceschi/rinha-de-backend-2026/issues/4644`.
- Resultado oficial Rust: `p99=1.76ms`, erro `0%`, `final_score=5754.17`, commit `45f8f63`.

## Experimento local

Implementação inicial:

- Runtime API em Rust.
- `serde_json` para parser inicial.
- Leitura do `index.bin` IVF já validado na submissão C++.
- Resposta via Unix socket atrás de `jrblatt/so-no-forevis:v1.0.0`.
- Correção de integração: suporte ao socket de controle `.ctrl` com `SCM_RIGHTS` para receber FDs repassados pelo LB.

Validações:

- `cargo build --release`: passou.
- Docker image local: `ghcr.io/viniciusdsandrade/rinha-de-backend-2026-rust:submission-rust`.
- `/ready` via LB: `HTTP/1.1 204 No Content`.
- Imagem publicada no GHCR com digest `sha256:b4c670863292deb1e0aca1056223e34900052998dcb4e8f69ab4169cb4052121`.
- Imagem pública usada na submissão: `ghcr.io/viniciusdsandrade/rinha-de-backend-2026:submission-rust-ivf`.

Resultado k6 local:

| Variante | p99 local | p90 local | HTTP errors | Erro local ponderado | final_score local | Leitura |
|---|---:|---:|---:|---:|---:|---|
| Rust IVF inicial com `.ctrl` | 3.41ms | 2.10ms | 0 | 474 | 3144.56 | funcional, ainda abaixo do C++ |
| Rust IVF com limite oficial de 350MB | 3.03ms | 2.01ms | 0 | 474 | 3196.16 | memória corrigida, elegível para nova execução oficial |
| Respostas HTTP como fatias estáticas | 5.61ms | 2.38ms | 0 | 474 | 2928.31 | rejeitado e revertido; piorou p99 |
| Fast path sem `Vec` para `nprobe=1` | 3.09ms | 1.86ms | 0 | 474 | 3187.17 | rejeitado e revertido; sem ganho claro |
| CPU APIs/LB `0.45/0.45/0.10` | 3.24ms | 1.91ms | 0 | 474 | 3166.83 | rejeitado e revertido; LB mais apertado piorou p99 |
| CPU APIs/LB `0.40/0.40/0.20` | 3.26ms | 2.07ms | 0 | 474 | 3163.11 | rejeitado e revertido; APIs mais apertadas pioraram p99 |
| Build Rust `target-cpu=x86-64-v3` | 2.85ms / 3.13ms / 3.05ms | 1.80ms / 1.91ms / 1.92ms | 0 | 474 | 3223.14 / 3181.58 / 3193.48 | rejeitado por instabilidade local; sem ganho sustentado |
| Volume de sockets em `tmpfs` | 3.65ms | 1.93ms | 0 | 474 | 3115.16 | rejeitado e revertido; piorou p99 |
| Allocator global `mimalloc` | 3.39ms | 1.89ms | 0 | 474 | 3146.75 | rejeitado e revertido; piorou p99 |
| Threadpool FD `512` | 3.12ms | 1.89ms | 0 | 474 | 3183.09 | rejeitado; reduziu criação de threads, mas não ganhou score |
| Threadpool FD `128` | 3.08ms | 1.88ms | 0 | 474 | 3189.85 | rejeitado e revertido; sem ganho sustentado |
| `serde_json` com strings emprestadas | 3.46ms | 1.89ms | 0 | 474 | 3138.83 | rejeitado e revertido; piorou p99 |
| LB `jrblatt/so-no-forevis:v0.0.2` | 3.51ms | 2.17ms | 0 | 474 | 3129.50 | rejeitado e revertido; `v1.0.0` segue melhor localmente |
| Build Rust Haswell explícito | 3.50ms | 1.91ms | 0 | 474 | 3133.80 | rejeitado e revertido; piorou p99 |
| Scan IVF AVX2 simples sem early-prune | 3.15ms | 1.81ms | 0 | 474 | 3179.39 | rejeitado e revertido; precisão ok, mas p99 não ganhou |
| Warmup sintético do índice | 5.02ms | 1.97ms | 0 | 474 | 2977.07 | rejeitado e revertido; aqueceu caminho ruim e piorou p99 |
| `IVF_BBOX_REPAIR=false` | 4.58ms | 1.93ms | 0 | 479 | 3011.19 | rejeitado e revertido; piorou p99 e erro ponderado |
| `IVF_BOUNDARY_FULL=false` | 4.39ms | 2.72ms | 0 | 474 | 3035.71 | rejeitado e revertido; preservou erro, mas piorou p99 |
| 1 API Rust `0.84 CPU/320MB` + LB `0.16 CPU/30MB` | 3.15ms | 1.88ms | 0 | 474 | 3179.40 | rejeitado e revertido; concentrar CPU em uma API piorou p99 contra o baseline 2 APIs |
| Rust TCP direto `1 CPU/350MB`, sem LB | 3.37ms | 1.90ms | 0 | 474 | 3150.28 | rejeitado e revertido; remover LB/FD passing piorou p99 local |
| Parser `simd-json` via serde | 3.47ms | 1.90ms | 0 | 474 | 3137.72 | rejeitado e revertido; custo no hot path piorou p99 contra `serde_json` atual |
| Ordem de dimensões por variância no scan int16 | 4.68ms | 2.00ms | 0 | 474 | 3007.58 | rejeitado e revertido; pruning ficou pior que a ordem natural |
| LB `so-no-forevis` com `WORKERS=2` | n/a | n/a | n/a | n/a | n/a | rejeitado e revertido; `/ready` ficou sem resposta localmente |
| `ulimits nofile=65535` em API e LB | 3.19ms | 1.92ms | 0 | 474 | 3173.94 | rejeitado e revertido; sem ganho contra baseline |

Resultado oficial:

| Issue | Commit | p99 oficial | HTTP/errors | final_score oficial | Leitura |
|---|---|---:|---:|---:|---|
| `#4644` | `45f8f63` | 1.76ms | 0 | 5754.17 | baseline Rust aceito; ainda abaixo do C++ `andrade-cpp-ivf` |

## Observacoes das stacks melhores

- `fksegundo/rinha-rust`: Rust com indice especialista exato, `mmap`, build-time preprocessing, FD passing, LB proprio e pool de threads com stack menor.
- `jairoblatt/rinha-2026-rust`: Rust com `monoio`/io_uring, `mimalloc`, AVX2/FMA explicito, parser HTTP manual e `so-no-forevis`.
- `rafaelcoelhox/eu-sou-o-ze-pamonha`: C com LB proprio, epoll, SCM_RIGHTS, AVX2/FMA e IVF k-means.
- `atomosdovini/rinha-fraud-cpp`: C++ com io_uring, AVX2/FMA, indice mmap e LB proprio.
- `muanlartins/rinha-de-backend-2026`: Go com mmap/madvise, raw HTTP, FD passing documentado e warmup.

Leitura para a Rust propria:

- Trocas superficiais de allocator, LB, CPU split e pequenas alocacoes nao foram suficientes.
- O gap restante parece estar no desenho do runtime de IO e no kernel de busca com pruning, nao em uma flag isolada.
- Proximas apostas com melhor relacao risco/retorno: parser especializado seguro, warmup de queries e SIMD com early-prune preservando ordenacao.

## Decisão

- A stack Rust foi registrada oficialmente como submissão paralela.
- A correção de memória elimina o bloqueio objetivo da primeira issue oficial.
- A Rust já tem baseline oficial válido, mas ainda não ameaça a submissão C++ atual, porque o p99 oficial está acima do C++ baseline.
- Próximo foco técnico: reduzir overhead de thread por conexão ou implementar SIMD no caminho IVF.
