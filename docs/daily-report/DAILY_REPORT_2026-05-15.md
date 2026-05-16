# Daily Report - 2026-05-15 - Rust IVF paralelo

## Objetivo

- Criar uma submissão paralela em nova linguagem sem alterar a submissão C++ atual `andrade-cpp-ivf`.
- Linguagem escolhida: Rust.
- Justificativa: Rust mantém controle fino de memória, sockets Unix, baixo overhead e caminho viável para SIMD/mmap, com iteração mais rápida que C/Zig para uma segunda stack.

## Registro oficial

- PR oficial aberto: `https://github.com/zanfranceschi/rinha-de-backend-2026/pull/4636`.
- Novo ID: `andrade-rust-ivf`.
- Repo público: `https://github.com/viniciusdsandrade/rinha-de-backend-2026-rust`.
- O primeiro run do PR falhou porque o repo Rust tinha apenas uma branch.
- Correção aplicada:
  - `main` com código fonte Rust.
  - `submission` com somente `docker-compose.yml` e `info.json`.
  - Novo push no PR para reexecutar validação.

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

Resultado k6 local:

| Variante | p99 local | p90 local | HTTP errors | Erro local ponderado | final_score local | Leitura |
|---|---:|---:|---:|---:|---:|---|
| Rust IVF inicial com `.ctrl` | 3.41ms | 2.10ms | 0 | 474 | 3144.56 | funcional, ainda abaixo do C++ |

## Decisão

- A stack Rust já é funcional e válida para smoke test.
- Ainda não é candidata para issue oficial de performance, porque o p99 local está pior que o C++ baseline.
- Próximo foco técnico: substituir `serde_json` por parser manual equivalente ao C++ ou reduzir overhead de thread por conexão.
