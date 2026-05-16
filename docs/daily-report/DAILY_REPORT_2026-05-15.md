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

## Decisão

- A stack Rust foi registrada oficialmente como submissão paralela.
- A correção de memória elimina o bloqueio objetivo da primeira issue oficial.
- Ainda não ameaça a submissão C++ atual, porque o p99 local permanece bem acima do C++ baseline.
- Próximo foco técnico: reduzir overhead de thread por conexão ou implementar SIMD no caminho IVF.
