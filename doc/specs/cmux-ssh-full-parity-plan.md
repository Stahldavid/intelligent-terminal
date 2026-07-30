# Plano de paridade completa com o cmux SSH

**Status:** implementação em andamento; paridade ainda não declarada

**Data:** 2026-07-30

**Produto:** Intelligent Terminal para Windows

**Escopo de referência:** paridade funcional com
[`cmux.com/docs/ssh`](https://cmux.com/docs/ssh), preservando como diferenciais
o Chat Pane ACP por surface, `wta team` e o control plane distribuído.

### Snapshot verificável da implementação

Este quadro evita confundir código compilado, teste em WSL e evidência em uma
máquina SSH física. `Parcial` significa que existe implementação útil, mas o
critério integral do requisito ainda não foi observado.

| ID | Estado em 2026-07-30 | Evidência atual | Gate restante |
|---|---|---|---|
| SSH-P01 | Parcial | fluxo Remote Workspace, store e UI implementados; harness instalado completou trust, bootstrap e sessões em `do-codex` | jornada integral criada pela UI contra host físico |
| SSH-P02 | Parcial | parser, `Include`, aliases concretos e `ssh -G` implementados/testados | comparação em matriz física de configurações |
| SSH-P03 | Parcial | preview/trust físico e rejeição de option injection implementados | alteração real de host key e deep links hostis |
| SSH-P04 | SSH físico aprovado | bootstrap versionado, SHA-256 e ativação atômica observados em `do-codex` | matriz adicional de OS/arquitetura |
| SSH-P05 | SSH físico aprovado | PTY preservou session ID, PID e backlog após queda forçada e reattach | TUI longa e suspensão do cliente |
| SSH-P06 | SSH físico aprovado | reconnect ao runtime original e backoff 3/6/12/24/48/60 implementados | jitter/packet loss prolongado |
| SSH-P07 | WSL aprovado | multi-attach e resize automático aprovados | TUI longa em rede física |
| SSH-P08 | Implementado em source; E2E parcial | proxy SOCKS5 loopback-only atravessou HTTP, HTTPS e WebSocket no host físico; Browser Surface nativa consome o proxy para um Remote Workspace pronto | jornada instalada de browser durante queda/reconnect |
| SSH-P09 | Implementado em preview; gate externo aberto | perfil WebView2 por surface, proxy scoped e policies fail-closed estão no host nativo | cookies, profile cleanup e abuso cross-workspace pela UI instalada |
| SSH-P10 | SSH físico parcial | matriz 0 B, Unicode, 1 MiB e cancelamento de 512 MiB passou com estado final `cancelled` e cleanup remoto; upload/download físico preservou SHA-256 | cancelamento durante rede degradada |
| SSH-P11 | Backend WSL aprovado | capability curta, journal entre attachments, replay/cross-surface e uso após revogação rejeitados | transporte físico e abuso cross-workspace pela UI instalada |
| SSH-P12 | Parcial | relay, unread/cooldown e jump por IDs exatos implementados | notificação física → badge/jump instalado |
| SSH-P13 | SSH físico aprovado para Codex | duas sessões Codex ACP autenticadas, isoladas e reanexadas com PIDs estáveis | matriz Claude/Gemini e troca rápida de foco pela UI |
| SSH-P14 | Parcial | tasks, ownership, dependências e heartbeat existem | team remoto atravessando queda física |
| SSH-P15 | Parcial | layout e bindings canônicos são reidratados | restart instalado retomando processos reais |
| SSH-P16 | Implementado | `doctor ssh/surface/agent`, reconcile/stop exatos e bundle redigido passaram no harness físico | UX final e suporte operacional prolongado |
| SSH-P17 | Parcial | transporte stdio, trust e capabilities fail-closed existem | auditoria física de listeners, secrets e revogação |
| SSH-P18 | Parcial | Windows x64 e helper Linux x64 entram no payload com hashes | matriz de plataformas e falha instalada por arch |

Evidências executadas nesta revisão:

- `cargo test` em `tools/wta`: 70 testes de biblioteca e 1226 testes do
  binário aprovados, 1296 no total, sem falhas;
- E2E WSL de PTY persistente: mesmo processo disponível após detach/attach;
- E2E SSH físico em `do-codex`: bootstrap idempotente do helper Linux x64
  com SHA-256
  `9befdff0fc7b24c05a76a7dfc621067b9e7034c5f62d9d37b8d5badcc844d055`;
- E2E SSH físico: PTY `physical-pty-6870f17b2a434194` preservou o PID
  `2227784`, o session ID e o backlog após queda forçada e reattach;
- E2E SSH físico: duas sessões Codex ACP autenticadas usaram PIDs distintos
  (`2228059` e `2228718`), preservaram os PIDs após reattach e não
  compartilharam stream;
- upload `transfer-5c55cdcb-6392-49aa-b198-104184f0d49a` e download
  `transfer-880e514f-50cf-4773-a92e-d0427da82ab2` terminaram com o mesmo
  SHA-256
  `3503a708568a5bb3aa67ff96793c497f28526294e7ec96236eb870110c513eb1`;
- bundle de evidência redigido foi gerado e validado sem credentials, source
  paths ou environment secrets;
- E2E WSL de transferência: commit atômico e SHA-256
  `8655a32fb2b9bdb0f4f09b204d43e75e5fcfe13715b18ba8f3102a85e7c98d64`;
- matriz SSH física `transfer-matrix-70de7cee4fb444fea2850b6176d44fae`:
  0 B, nome Unicode e 1 MiB concluídos; cancelamento de 512 MiB terminou
  `cancelled` e removeu o temporário remoto;
- proxy SSH físico `proxy-e2e-e5caccfb5e5d472eb0f54ee2274fcde8`:
  bind somente `127.0.0.1`, HTTP/HTTPS/WebSocket e localhost remoto aprovados;
  encerramento forçado do supervisor também encerrou o processo SSH/porta e o
  reconcile marcou a sessão como falha;
- relay WSL `relay-workspace-e21bf59d761d48218f21d5dbf8b331e0`:
  journal sobreviveu a novo attachment; replay, cross-surface e uso após
  revogação foram rejeitados;
- builder Azure efêmero `run-20260729-012330-5f5588cc`: par WTA x64 Release
  criado antes do MSIX, quatro estágios MSBuild com zero erros, setup
  `0.9.4.12` de 24.241.339 bytes com SHA-256
  `44b6025c9eb4ec22ad0e80eb2dc506652eec90b36c167a7f495f2a4094cfa447`,
  protocolo `3.1`, manifesto/payloads verificados e VM desalocada;
- `TerminalControlLib` e `TerminalAppLib` Release x64 compilados;
- instalador Release x64 criado a partir do mesmo source state, reinstalado e
  validado com hashes do payload;
- handshake autenticado `wtcli` executado dentro de uma surface instalada:
  `connected=true`, protocolo `3.1`;
- E2E instalado de surface: contagem `1 → 2 → 3 → 4 → 1`, perfil Command
  Prompt heterogêneo respeitado e cleanup da Managed Agent Surface removeu o
  binding e todos os leases;
- os gates físicos básicos de bootstrap, PTY, Codex ACP, upload/download,
  volume/cancelamento cooperativo e proxy HTTP/HTTPS/WebSocket foram fechados;
  Browser Surface tem implementação nativa em preview, mas host-key rotation,
  relay pela UI física, matriz multi-adapter, isolamento/cleanup WebView2 e a
  jornada UI integral continuam abertos e não contam como paridade total.

## 1. Resultado pretendido

O usuário deve conseguir escolher um host OpenSSH e obter, em uma única
operação, um **Remote Workspace** completo:

```text
Conectar
  → validar confiança e capacidades
  → criar workspace remoto
  → abrir terminais/agentes
  → dividir panes e criar surfaces
  → usar browser e arquivos pela rede remota
  → receber notificações locais
  → perder a conexão
  → reconectar às mesmas sessões
  → continuar sem misturar agentes ou worktrees
```

O fluxo normal não deve exigir que o usuário compreenda a diferença entre
`ssh.exe`, bootstrap, `wta-node`, binding, lease, ACP ou relay. Esses conceitos
continuam explícitos nos diagnósticos e APIs, mas formam uma única experiência
de produto.

Paridade não significa copiar a arquitetura interna do cmux. O Intelligent
Terminal continuará usando:

- Windows Terminal, WinUI e ConPTY;
- OpenSSH como transporte;
- `wta-node` como runtime remoto versionado;
- ACP para conversas de agentes;
- Compute Store como fonte canônica;
- Terminal Protocol com capabilities;
- `wta team` para coordenação agent-neutral.

## 2. Fontes e baseline

### 2.1 Referências externas

O contrato de paridade é derivado das seguintes páginas:

- [cmux SSH](https://cmux.com/docs/ssh);
- [cmux Concepts](https://cmux.com/docs/concepts);
- [cmux Notifications](https://cmux.com/docs/notifications);
- [cmux Session Restore](https://cmux.com/docs/session-restore);
- [cmux CLI Reference](https://cmux.com/docs/api).

Se as docs mudarem durante a implementação, cada milestone deve registrar a
versão/data consultada e classificar a diferença como:

- obrigatória para o baseline `/docs/ssh`;
- adjacente ao SSH;
- diferencial opcional;
- explicitamente fora de escopo.

### 2.2 Estado observado no Intelligent Terminal

Já existem no checkout:

- hierarquia canônica `Window → Workspace → Pane → Surface`;
- surfaces heterogêneas dentro de cada pane;
- Chat Pane ligado automaticamente à surface focada;
- OpenSSH Provider com aliases concretos, `Include`, `ssh -G`, `ProxyJump` e
  proteção contra option injection;
- targets SSH descobertos como `restricted` e `disabled`;
- `wta-node` Windows e Linux x86_64;
- bootstrap versionado com SHA-256;
- JSON-RPC por stdio;
- sessões ACP persistentes com `start`, `attach`, `detach`, `stop` e `list`;
- `remote_session_id` estável por surface;
- placement sticky, leases, snapshots, jobs e handoff;
- `wta team` com tasks, ownership, dependências e heartbeat;
- Terminal Protocol autenticado e capability-scoped.

O E2E WSL comprovou duas sessões Codex ACP com PIDs isolados e estáveis após
detach/attach. O harness `do-codex` comprovou bootstrap, reconnect ao mesmo
runtime, duas sessões Codex autenticadas e isoladas, transferência em volume,
cancelamento cooperativo e proxy loopback HTTP/HTTPS/WebSocket. Ainda não foram
observados em um host SSH físico:

- alteração real de host key;
- duas Managed Agent Surfaces alternadas pela UI durante streaming;
- relay remoto projetado na UI instalada;
- Browser Surface/WebView2 com cookies e policy isolados;
- adapters Claude/Gemini autenticados.

No runtime consultado em 2026-07-28 havia:

- 13 targets descobertos;
- 1 target local saudável;
- 2 targets WSL;
- 10 targets SSH restritos, desabilitados e ainda sem probe;
- 3 bindings `plain_terminal`;
- 0 Managed Agent Surfaces, leases ou jobs ativos.

Produção não será usada para fechar os gates físicos.

## 3. Definição estrita de paridade

O produto só poderá declarar **cmux SSH parity** quando todos os requisitos
abaixo forem observados em builds instaláveis.

| ID | Capacidade obrigatória | Critério resumido |
|---|---|---|
| SSH-P01 | Remote Workspace em uma operação | selecionar host cria workspace remoto gerenciado e focado |
| SSH-P02 | OpenSSH config | aliases, identity, port, `Include`, proxy e precedência são respeitados |
| SSH-P03 | Trust seguro | preview, confirmação, host key e deep links fail-closed |
| SSH-P04 | Helper remoto | probe OS/arch, upload versionado, SHA-256 e ativação atômica |
| SSH-P05 | PTY persistente | shell/TUI continua vivo quando o transporte cai |
| SSH-P06 | Reconnect | backoff 3/6/12 até 60 s e reattach à mesma sessão |
| SSH-P07 | Resize/multi-attach | tamanho efetivo determinístico, sem corrupção de TUI |
| SSH-P08 | Browser remoto | HTTP/WebSocket e `localhost` atravessam a rede remota |
| SSH-P09 | Cookies isolados | perfil de browser separado por Remote Workspace |
| SSH-P10 | Drag-and-drop | arquivo local é enviado, validado e inserido no terminal remoto |
| SSH-P11 | Relay remoto-local | comandos autorizados no host remoto controlam a instância local |
| SSH-P12 | Notificações | unread, badge, cooldown e jump para workspace/surface |
| SSH-P13 | Agentes remotos | Codex/Claude/outro ACP sobrevivem a reconnect sem troca de conversa |
| SSH-P14 | Teams remotos | workers aparecem nas surfaces corretas e preservam ownership |
| SSH-P15 | Session restore | layout volta; processos retomam apenas por sessão persistente/resume seguro |
| SSH-P16 | Diagnóstico | estado, falha, retry, target e session ID são inspecionáveis |
| SSH-P17 | Segurança | nenhum ACP/app-server fica público; capabilities são mínimas e revogáveis |
| SSH-P18 | Empacotamento | helper correto existe para toda plataforma declarada como suportada |

### 3.1 O que não conta como paridade

Não são evidência suficiente:

- unit tests sem host físico;
- mocks de SSH;
- abrir `ssh.exe` em uma surface comum;
- iniciar o helper sem reattach real;
- reabrir o layout sem recuperar a sessão correspondente;
- detectar um agent CLI sem executar operação autenticada;
- fazer upload de um arquivo pequeno sem validar cancelamento, checksum e
  cleanup;
- exibir estado otimista na UI sem confirmação do node;
- validar somente WSL e descrever o resultado como SSH remoto.

## 4. Jornadas críticas

### Jornada A — primeiro acesso

1. Usuário escolhe **New remote workspace…**.
2. O seletor mostra aliases concretos do OpenSSH e targets previamente
   confiados.
3. O produto mostra destino resolvido, usuário, porta, jump host, política de
   host key e capabilities solicitadas.
4. O usuário confirma confiança.
5. O terminal executa probe e bootstrap.
6. Um workspace remoto é criado com uma surface terminal inicial.
7. A sidebar mostra target, status e latência sem expor secrets.

### Jornada B — trabalho remoto e queda

1. Usuário abre shell, Codex e logs em surfaces distintas.
2. A conexão é interrompida.
3. O conteúdo permanece visível e recebe overlay de reconnect.
4. O node mantém PTYs e agentes vivos.
5. O transporte reconecta.
6. Cada surface reanexa ao mesmo `remote_session_id`.
7. Input enviado durante desconexão não é perdido nem duplicado
   silenciosamente.

### Jornada C — preview e arquivos

1. Aplicação remota inicia em `localhost:3000`.
2. Browser da surface usa a rede do Remote Workspace.
3. HTTP e WebSocket funcionam sem `ssh -L` manual.
4. Usuário arrasta uma imagem para uma surface terminal.
5. O arquivo é enviado com progresso, checksum e destino previsível.
6. O caminho remoto é inserido como bracketed paste somente após sucesso.

### Jornada D — agentes e coordenação

1. Usuário cria duas Managed Agent Surfaces no mesmo Remote Workspace.
2. Cada surface mantém processo, transcript e Chat Pane próprios.
3. Trocar foco troca a conversa sem misturar streams.
4. `wta team` cria workers em panes/surfaces identificáveis.
5. Uma queda SSH não troca ownership, task ID, worktree ou HomeTarget.
6. Ações que exigem aprovação voltam à surface e ao usuário corretos.

## 5. Arquitetura-alvo

```text
Intelligent Terminal
  ├─ Workspace/Pane/Surface UI
  ├─ Remote Workspace Controller
  │    ├─ Trust & Connection Wizard
  │    ├─ Attachment/Reconnect Manager
  │    ├─ Transfer Manager
  │    ├─ Browser Proxy Client
  │    └─ Notification/CLI Relay Gateway
  ├─ Chat Pane / wta-master
  ├─ wta team
  ├─ wta compute / Compute Store
  └─ OpenSSH transport
        └─ wta-node
             ├─ PTY Session Manager
             ├─ ACP Session Manager
             ├─ File Transfer Service
             ├─ SOCKS5/CONNECT Proxy
             ├─ Reverse Relay
             └─ Resource/Job Runtime
```

### 5.1 Decisões arquiteturais obrigatórias

1. **SSH não armazena estado.** Estado canônico permanece no Compute Store e
   no registry do node.
2. **PTY e ACP são tipos de sessão distintos.** ACP não deve ser usado para
   manter um shell genérico.
3. **Plain SSH continua disponível**, mas será rotulado como conexão não
   gerenciada. O fast path recomendado será Remote Workspace.
4. **Não depender de ControlMaster no Windows.** O canal `wta-node` deve
   multiplexar controle, PTY, proxy e eventos. `scp`/SFTP pode abrir transporte
   próprio quando necessário.
5. **Não interceptar comandos arbitrários.** Routing de jobs continua
   explícito.
6. **Chat Pane segue a surface.** Não reintroduzir seletor
   Workspace/Surface/Team.
7. **Um writer por worktree.** Reconnect nunca cria writer duplicado.
8. **Browser e relay são capability-gated.** Hosts não confiados não recebem
   essas capacidades.
9. **Sem endpoint público.** Todo controle passa por SSH/stdio ou socket local
   protegido no host remoto.
10. **Falha de host key é terminal.** Não aplicar retry automático.

## 6. Entidades e contratos necessários

O modelo existente deve ser estendido sem criar uma hierarquia paralela.

### `RemoteWorkspaceSession`

- IDs canônicos de window/workspace;
- `target_id`;
- versão/protocolo do node;
- estado da conexão;
- capabilities negociadas;
- política de reconnect;
- timestamps de connect/disconnect;
- último erro sanitizado;
- IDs das surfaces remotas.

### `RemoteSurfaceSession`

- `workspace_id`, `pane_id`, `surface_id`;
- tipo `pty | managed_agent | browser`;
- `remote_session_id`;
- cwd e profile declarados;
- estado de attachment;
- dimensões de terminal;
- resume binding, quando aplicável;
- nunca contém transcript, token ou argv secreto.

### `RemoteAttachment`

- ID efêmero;
- surface/session correspondente;
- geração monotônica;
- dimensões;
- cursor de backlog;
- capability de input;
- heartbeat/expiry.

### `RemoteTransfer`

- origem local e destino remoto sanitizados;
- tamanho e SHA-256;
- status/progresso;
- política de overwrite;
- ID da surface que originou a ação;
- cleanup registrado para falha/cancelamento.

### `RemoteRelayCapability`

- workspace e surface permitidos;
- operações permitidas;
- issuer, audience, expiração e nonce;
- segredo/chave somente na memória ou storage protegido;
- revogação no detach/close.

### Estados da conexão

```text
discovered
  → awaiting_trust
  → probing
  → bootstrapping
  → connecting
  → connected
  → reconnecting
  → connected

Falhas terminais:
  host_key_changed
  authentication_required
  policy_denied
  incompatible_node
  unsupported_platform
  user_cancelled
```

Falhas terminais não entram em loop de retry.

### Estados de uma surface remota

```text
creating → running → attached
                    ↘ detached → attached
                    ↘ exited
                    ↘ failed
```

`transport disconnected` não equivale a `surface exited`.

## 7. Plano de implementação por milestones

### P0 — Contrato de paridade e harness físico

**Objetivo:** tornar o risco externo reproduzível antes de ampliar a UI.

#### Entregas

- checklist SSH-P01–P18 versionado;
- inventário das APIs atuais do `wta-node`;
- fixtures OpenSSH com `Include`, `ProxyJump`, identities, `RemoteCommand`,
  `RequestTTY`, aliases inválidos e host-key change;
- devbox Linux x86_64 dedicado e não produtivo;
- usuário remoto sem privilégios administrativos;
- script E2E idempotente com setup, fault injection e cleanup;
- captura estruturada de PID, session ID, hash, target e reconnect generation;
- ADR para PTY persistente e multiplexação do canal.

#### Gate mínimo

- probe, bootstrap e handshake reais no devbox;
- mudança simulada de host key falha fechada;
- nenhum target de produção é habilitado;
- cleanup não deixa daemon, PTY, chave ou arquivo temporário órfão.

#### Bloqueio

Nenhuma declaração de paridade ou ativação default antes deste gate.

### P1 — Remote Workspace e trust unificados

**Objetivo:** substituir a sequência manual target/probe/bootstrap/workspace
por uma operação coerente.

#### Entregas

- comando/API `remote workspace create`;
- wizard **New remote workspace…**;
- preview do destino efetivamente resolvido por `ssh -G`;
- trust explícito e persistido por identidade do target/host key;
- probe e bootstrap como etapas observáveis;
- opção `--name`, port, identity e host-key policy;
- `--no-focus`;
- deep links com confirmação e allowlist de parâmetros;
- fallback explícito **Open plain SSH terminal**.

#### Regras para deep links

Links externos não podem fornecer:

- identity file;
- raw `-o`;
- command;
- `ProxyCommand`;
- forwarding;
- paths locais arbitrários.

Opções avançadas devem vir do OpenSSH config já confiado.

#### Gate mínimo

- primeiro acesso cria workspace remoto e terminal inicial;
- segundo acesso reutiliza trust e helper compatível;
- cancelamento em qualquer etapa não deixa workspace fantasma;
- aliases iniciados por `-`, wildcard ou negados são rejeitados.

### P2 — PTY remoto persistente

**Objetivo:** dar a shells e TUIs a mesma durabilidade já existente para ACP.

#### Entregas

- session manager genérico `pty start/attach/detach/stop/list`;
- ConPTY/PTY remoto fora do ciclo de vida do canal SSH;
- ring buffer limitado de output;
- cursor de backlog por attachment;
- input com geração/idempotência definida;
- exit code e reason persistidos;
- resize e smallest-screen-wins para múltiplos attachments;
- attach após reinício do transporte;
- kill explícito e cleanup por TTL/policy.

#### Gate mínimo

- iniciar shell e TUI no devbox;
- matar transporte SSH;
- confirmar mesmo PID e `remote_session_id`;
- reconectar e continuar interação;
- repetir o ciclo pelo menos 20 vezes sem duplicar input ou processo;
- duas surfaces não compartilham output, cwd ou resize state.

### P3 — Reconnect e lifecycle do workspace

**Objetivo:** fazer queda de rede parecer uma pausa, não uma perda de sessão.

#### Entregas

- attachment manager por Remote Workspace;
- backoff 3, 6, 12, 24, 48 e máximo 60 segundos;
- keepalive configurável, com default compatível quando OpenSSH config não
  define valor;
- overlay não destrutivo `Disconnected / Reconnecting`;
- ações `Retry now`, `Open diagnostics`, `Detach`, `Stop remote session`;
- supressão de retry para auth, policy, host key e incompatibilidade;
- recuperação independente por surface;
- shutdown ordenado ao fechar workspace.

#### Gate mínimo

- queda física retoma shell, Codex e logs;
- surface encerrada remotamente não é “ressuscitada”;
- auth rejeitada solicita ação do usuário uma vez;
- fechar workspace não mata sessão sem confirmação quando detach é permitido.

### P4 — Relay remoto-local e notificações

**Objetivo:** permitir que agentes e scripts remotos sinalizem e controlem
somente o workspace/surface autorizados.

#### Entregas

- reverse relay multiplexado pelo canal do node;
- handshake autenticado e binding a workspace/surface;
- capabilities curtas e revogáveis;
- subset inicial: notify, set/clear status, progress, focus/jump e listagem
  escopada;
- tradução segura para Terminal Protocol;
- painel de notificações;
- unread por workspace/surface;
- jump-to-surface;
- cooldown por target e deduplicação;
- hooks locais e remotos com timeout e comportamento fail-safe.

#### Gate mínimo

- processo remoto notifica a surface correta em menos de 1 segundo na rede de
  teste;
- processo em outra surface não consegue enviar input, renomear ou focar sem
  capability;
- token expirado/reutilizado é rejeitado;
- relay nunca escuta publicamente.

### P5 — Drag-and-drop e transferências

**Objetivo:** transformar o drop local em upload remoto previsível e seguro.

#### Entregas

- roteamento do `TermControl` conforme `RemoteSurfaceSession`;
- seletor/política de diretório remoto;
- transfer manager com progresso e cancelamento;
- SFTP/SCP ou data channel separado do JSON-RPC;
- SHA-256 e tamanho verificados;
- overwrite explícito;
- arquivo temporário + rename atômico;
- bracketed paste do path somente depois do sucesso;
- suporte a múltiplos arquivos;
- logs sem nomes sensíveis por default.

#### Gate mínimo

- arquivos de 0 B, pequeno, 512 MB e nome Unicode;
- cancelamento remove temporário;
- rede interrompida não produz arquivo final truncado;
- path traversal e symlink escape falham fechados;
- throughput medido contra `scp` baseline.

### P6 — Browser pela rede remota

**Objetivo:** fazer `localhost` e WebSocket remotos funcionarem sem forwarding
manual.

#### Entregas

- SOCKS5 e HTTP CONNECT no protocolo do node;
- associação obrigatória a Remote Workspace;
- resolução DNS pela rede remota;
- proxy HTTP, HTTPS e WebSocket;
- Surface/Panel de browser;
- perfil WebView2 isolado por Remote Workspace;
- lifecycle e limpeza de cookie store;
- indicadores visíveis de target e origem;
- bloqueio de acesso indevido a redes de outros workspaces;
- feature flag e policy empresarial.

#### Gate mínimo

- `localhost` remoto, HTTP, HTTPS e WebSocket passam;
- dois workspaces no mesmo domínio não compartilham cookies;
- fechar workspace revoga proxy;
- browser não aceita relay/capability de outro workspace;
- proxy não vira SOCKS público.

#### Observação WebView2

A Browser Surface WebView2 existe em preview quando um Remote Workspace está
pronto. Ela usa perfil por surface, proxy scoped e policy fail-closed. Isso
fecha a implementação de source, mas não SSH-P09: isolamento de cookies,
cleanup, reconnect e ataques cross-workspace ainda precisam passar na UI
instalada antes de declarar paridade total.

### P7 — Agentes e teams remotos

**Objetivo:** preservar os diferenciais ACP/WTA sobre o transporte completo.

#### Entregas

- Managed Agent Surface criada diretamente dentro do Remote Workspace;
- bootstrap/doctor do adapter remoto;
- Codex, Claude e adapter custom compatível com ACP;
- autenticação reportada separadamente do health do node;
- reattach ao mesmo adapter/PID;
- aprovação e tool call retornando à surface correta;
- `wta team` usando os mesmos IDs e bindings;
- workers remotos em splits/surfaces nativos;
- ownership, heartbeat e shutdown após reconnect;
- nenhuma dependência de tmux shim no núcleo.

#### Gate mínimo

- dois Codex autenticados com PIDs e transcripts distintos;
- streaming + troca rápida de foco sem vazamento;
- queda SSH + reattach autenticado;
- coordinator opcional observa workers sem sequestrar o Chat Pane;
- shutdown limpa workers sem matar sessões não pertencentes ao team.

### P8 — Session restore e continuidade

**Objetivo:** restaurar layout e retomar somente aquilo que tem contrato de
resume seguro.

#### Entregas

- snapshot versionado de window/workspace/pane/surface;
- target e `remote_session_id` sem secrets;
- restore de layout, cwd, metadata e browser state;
- reattach a sessão ainda viva;
- fallback para resume nativo do agent quando a sessão morreu;
- comandos de resume com prefixo aprovado e environment sanitizado;
- opção global e por workspace para não auto-resumir;
- restore manual da sessão anterior.

#### Gate mínimo

- reiniciar app e reconstruir layout;
- reanexar sessão remota viva sem criar processo novo;
- sessão morta só é reiniciada com binding confiável;
- tokens, passwords e API keys não aparecem no snapshot;
- restore corrompido falha sem apagar o original.

### P9 — Hardening, observabilidade e release

**Objetivo:** tornar a paridade suportável como produto, não apenas demo.

#### Entregas

- `wta remote doctor`;
- status consolidado no Agents & Tasks;
- logs correlacionados por connection/workspace/surface/session;
- export de diagnóstico com redaction;
- métricas de connect, bootstrap, reconnect, transfer e relay;
- testes de upgrade/downgrade do node;
- compatibilidade de protocolo N/N-1;
- atualização e rollback do helper;
- documentação de usuário e runbook;
- localization e accessibility;
- installer contendo todos os helpers suportados;
- release checklist SSH-P01–P18.

#### Gate mínimo

- matriz completa verde em duas máquinas físicas;
- zero uso de produção durante validação;
- rollback para versão anterior preserva store ou falha com mensagem clara;
- processo órfão, secret em log, listener público ou host-key bypass bloqueiam
  release.

## 8. Matriz de testes obrigatória

### Plataformas mínimas

| Cliente | Host remoto | Status necessário |
|---|---|---|
| Windows 11 x64 | Ubuntu 22.04/24.04 x64 | obrigatório |
| Windows 11 x64 | Windows 11/Server x64 OpenSSH | obrigatório antes de “total” |
| Windows 11 ARM64 | Linux ARM64 | somente se o instalador anunciar suporte |
| Windows 10 | qualquer | conforme suporte oficial do produto |

Plataforma sem helper empacotado deve falhar como `unsupported_platform`,
nunca tentar executar binário incompatível.

### Cenários de transporte

- conexão direta;
- `ProxyJump`;
- identity configurada no arquivo;
- porta não padrão;
- latência 100–300 ms;
- perda de pacotes;
- queda total;
- mudança de IP;
- reinício do sshd;
- host key alterada;
- auth expirada;
- `RemoteCommand` e `RequestTTY` existentes.

### Cenários de sessão

- shell ocioso;
- comando produzindo output contínuo;
- TUI full-screen;
- Codex em streaming;
- duas e dez surfaces;
- dois attachments;
- resize concorrente;
- detach longo;
- node upgrade com sessões existentes;
- app local reiniciado.

### Segurança negativa

- alias `-oProxyCommand=...`;
- deep link com command/identity/forwarding;
- replay de capability;
- relay cross-workspace;
- path traversal no upload;
- symlink para fora do destino;
- helper com hash incorreto;
- node incompatível;
- JSON-RPC oversized/malformado;
- backlog excedido;
- target `production` selecionado por `auto`.

## 9. Budgets de qualidade

Os valores finais devem ser confirmados em P0, mas estes são os budgets
iniciais:

| Métrica | Budget inicial |
|---|---:|
| UI permanece responsiva durante connect/bootstrap | nenhum bloqueio >100 ms na thread UI |
| Estado de disconnect visível | <500 ms após detecção |
| Primeiro retry | 3 s |
| Reattach após transporte disponível | <5 s em rede de teste |
| Notificação remota → UI | p95 <1 s |
| Troca de Chat Pane entre sessions prontas | p95 <200 ms |
| Upload | ≥80% do throughput de `scp` equivalente |
| Memória de backlog | limitada e configurável por session/workspace |
| Processos órfãos após cleanup E2E | 0 |

## 10. Migração e compatibilidade

- Perfis SSH existentes continuam abrindo Plain SSH Surface.
- O menu passa a diferenciar:
  - **New remote workspace…**
  - **Open plain SSH terminal**
- Um plain SSH não é promovido silenciosamente a managed.
- A promoção pode existir como ação explícita após trust/probe.
- Workspaces existentes sem metadata remota continuam locais.
- Store recebe migration versionada e backup antes da primeira escrita.
- Node incompatível é atualizado somente após verificação e confirmação de
  policy.
- Fechar uma versão nova com uma antiga não pode corromper o store.

## 11. Mapa de componentes provável

As decisões finais de organização permanecem para a implementação, mas os
pontos de integração existentes são:

| Área | Código atual |
|---|---|
| OpenSSH discovery/probe | `tools/wta/src/compute/ssh.rs` |
| Compute models/store | `tools/wta/src/compute/model.rs`, `store.rs` |
| Sessões persistentes | `tools/wta/src/compute/session.rs` |
| Node JSON-RPC | `tools/wta/src/compute/node.rs`, `tools/wta/src/main.rs` |
| ACP remoto | `tools/wta/src/protocol/acp/spawn.rs` |
| Jobs/snapshots | `tools/wta/src/compute/execution.rs`, `snapshot.rs` |
| Managed Agent Surface | `src/cascadia/TerminalApp/TabManagement.cpp` |
| Surface UI | `SurfaceStackPaneContent.*` |
| Workspace/sidebar | `WorkspaceSidebar.cpp`, `TerminalPage.*` |
| Drag-and-drop | `src/cascadia/TerminalControl/TermControl.cpp` |
| Notificação nativa | `src/cascadia/TerminalApp/DesktopNotification.*` |
| Terminal Protocol | `TerminalPage.Protocol.cpp`, `TerminalProtocolComServer.*` |
| Empacotamento | `CascadiaPackage.wapproj`, `doc/building-installer.md` |

## 12. Riscos e mitigação

| Risco | Impacto | Mitigação/gate |
|---|---|---|
| PTY persistente corromper input/output | crítico | P2 antes de browser/UX avançada; fault injection |
| `ControlMaster` inconsistente no Windows | alto | não torná-lo dependência; multiplexar no node |
| Relay virar canal de controle amplo | crítico | capabilities curtas, scope e testes hostis |
| Proxy virar acesso público | crítico | somente canal SSH, bind local e revogação |
| WebView2 compartilhar cookies | alto | user-data-dir isolado por workspace |
| Reconnect criar segundo writer | crítico | lease + generation + HomeTarget sticky |
| Host de produção ser usado em teste | crítico | trust tier bloqueia; harness aceita somente allowlist dev |
| Helper quebrar após upgrade | alto | hash, protocolo N/N-1 e rollback |
| Backlog consumir disco/memória | alto | limites, truncation event e métricas |
| Upload parcial ser tratado como final | alto | temp + checksum + rename |
| Auth do agente ser confundida com SSH | médio | estados e diagnósticos separados |
| Escopo crescer para clone integral do cmux | médio | baseline fixo SSH-P01–P18 |

## 13. Itens deliberadamente adiados

Não bloqueiam a paridade estrita com `/docs/ssh`:

- Azure auto-start/deallocate e billing;
- CAS/chunk deduplication;
- uso de máquinas de produção como pool;
- compatibilidade `tmux -CC`/mirror;
- importação de cookies de browsers externos;
- file explorer remoto completo;
- checkpoint de memória de processos arbitrários;
- migração automática de agente vivo entre máquinas;
- liberação geral de WebView2 antes dos gates externos de P6;
- suporte a OS/arquitetura não empacotados.

Esses itens devem ser reavaliados somente depois de P9.

## 14. Ordem crítica e regra de release

```text
P0 harness físico
  → P1 trust/workspace
  → P2 PTY persistente
  → P3 reconnect
  → P4 relay/notificações
  → P5 transferências
  → P6 browser
  → P7 agentes/teams
  → P8 restore
  → P9 hardening/release
```

P4 e P5 podem ser desenvolvidos em paralelo depois de P3. P6 depende do canal
multiplexado e da policy de P4. P7 reutiliza P2/P3 e não deve criar um segundo
reconnect manager.

O release só recebe o rótulo **cmux SSH parity** quando:

1. SSH-P01–P18 estão verdes;
2. Linux x64 e Windows x64 remotos passaram em máquinas físicas;
3. Codex remoto autenticado passou após reconnect;
4. browser, upload, relay e notificações passaram em E2E;
5. nenhum gate foi substituído por mock;
6. segurança aprovou host key, relay, proxy, upload e secret handling;
7. rollback foi executado;
8. os resultados e limitações foram publicados no implementation report.

Até lá, a nomenclatura correta é:

> **Remote Workspace Preview — implementação parcial; SSH físico e recursos de
> paridade ainda sujeitos aos gates documentados.**

## 15. Rastreabilidade requisito → milestone → evidência

| Requisito | Milestone principal | Evidência obrigatória |
|---|---|---|
| SSH-P01 | P1 | vídeo/log do primeiro connect criando workspace e surface |
| SSH-P02 | P0/P1 | fixtures + `ssh -G` comparado ao OpenSSH executado |
| SSH-P03 | P0/P1 | testes negativos de host key, deep link e option injection |
| SSH-P04 | P0/P1 | hash local/remoto, versão e ativação atômica registrados |
| SSH-P05 | P2 | PID e session ID iguais antes/depois da queda |
| SSH-P06 | P3 | timeline de retries e reattach físico |
| SSH-P07 | P2 | teste de dois attachments e resize concorrente |
| SSH-P08 | P6 | HTTP/HTTPS/WebSocket/localhost remoto |
| SSH-P09 | P6 | teste cruzado demonstrando cookie isolation |
| SSH-P10 | P5 | matriz de arquivos, checksum, cancelamento e cleanup |
| SSH-P11 | P4 | chamadas permitidas e ataques cross-scope rejeitados |
| SSH-P12 | P4 | unread, cooldown e jump para IDs exatos |
| SSH-P13 | P7 | dois adapters autenticados, isolados e reanexados |
| SSH-P14 | P7 | team com ownership/heartbeat preservados após queda |
| SSH-P15 | P8 | restart do app, restore e resume seguro |
| SSH-P16 | P9 | doctor + bundle de diagnóstico redigido |
| SSH-P17 | todos/P9 | security suite e inspeção independente de listeners/secrets |
| SSH-P18 | P0/P9 | matriz de artefatos e falha clara para plataforma não suportada |

Cada execução E2E deve produzir um bundle contendo:

- versão do produto e protocolo;
- plataforma local/remota;
- target e session IDs sanitizados;
- timestamps e gerações de reconnect;
- PIDs antes/depois;
- hashes do helper e transferências;
- resultados estruturados dos assertions;
- logs redigidos;
- lista explícita de gates não executados.

Screenshots ou vídeos ajudam a comprovar UX, mas não substituem assertions de
processo, identidade, segurança ou persistência.

## 16. Organização em ciclos de entrega

Os milestones são gates técnicos, não promessa de prazo. Para planejamento de
capacidade, a sequência pode ser agrupada em quatro ciclos:

| Ciclo | Milestones | Resultado utilizável |
|---|---|---|
| A — fundação remota | P0–P2 | workspace confiado e PTY persistente no devbox |
| B — continuidade e interação | P3–P5 | reconnect, notificações e arquivos |
| C — experiência completa | P6–P7 | browser remoto e agentes/teams |
| D — durabilidade e release | P8–P9 | restore, hardening, packaging e declaração de paridade |

Cada ciclo termina com:

1. build instalável;
2. testes locais;
3. E2E físico aplicável;
4. security negatives;
5. atualização do implementation report;
6. demonstração do fluxo de usuário;
7. lista de deferrals e riscos residuais.

## 17. Checkpoint da vertical slice — 2026-07-29

O baseline implementado agora contém:

- contratos canônicos `ExecutionEnvironment`, `LaunchMethod`,
  `AccessEndpoint` e `EnvironmentConnectionSupervisor`;
- um supervisor por environment e backoff `3, 6, 12, 24, 48, 60` segundos;
- endpoints SSH/private ativos e tipos públicos/overlay/relay fail-closed;
- browser, files e node client consumindo o mesmo supervisor;
- restore por environment, target, binding, endpoint preferido e runtime ID,
  sem usar portas, PIDs, túneis ou credenciais como identidade;
- File Explorer com root opaco, relative path e capabilities separadas;
- HOME/admin opt-in, capability-gated e com reconhecimento visível;
- download imutável preparado pelo file RPC scoped e rota HOME antiga
  desabilitada;
- Browser Surface nativa com perfil WebView2 isolado, proxy SOCKS por surface,
  host objects/web messages/devtools/autofill/passwords/downloads desligados e
  navegação HTTP/HTTPS;
- `Agents & Tasks` consumindo environments, connections, roots redigidos e
  métricas reais do PTY no mesmo workspace context.

Evidência determinística:

```text
Verify-RemoteRuntimeVerticalSlice.ps1   PASS
Verify-TerminalProtocolSecurity.ps1     PASS
cargo test --lib                        70 passed
cargo check --all-targets               PASS
```

Isso fecha o contrato da vertical slice, não a declaração de release. O nome
continua **Remote Workspace Preview** até os gates físicos/instalados das
seções 14–16, especialmente cookie isolation cross-workspace, restore após
restart do app, host-key rotation e reconnect prolongado sob falhas de rede.
