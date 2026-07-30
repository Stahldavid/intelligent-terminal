# Relatório de implementação: control plane distribuído de agentes e compute

**Data:** 2026-07-28

**Branch:** `feature/agent-workspace-launcher`

**Plano de origem:**
[`distributed-agent-compute-control-plane-plan.md`](distributed-agent-compute-control-plane-plan.md)

**Resultado:** RC-P0–RC-P8 estão implementados no checkout e empacotados na
versão local `0.9.4.12`. Além da validação local em Windows e WSL, o harness
restrito ao devbox não produtivo `do-codex` observou bootstrap com hash,
persistência e reattach de PTY, duas sessões Codex ACP autenticadas e isoladas,
upload verificado, transferência/cancelamento em volume, proxy remoto
HTTP/HTTPS/WebSocket e export redigido. O backend do relay foi validado em WSL
com rejeição de replay/cross-surface/revogação. Na data deste snapshot,
Browser/WebView2 ainda não estava integrado. Relay projetado na UI física,
host-key rotation e matriz multi-adapter permaneciam gates explícitos; não
foram simulados nem declarados como observados.

> **Adendo de source — 2026-07-30:** as seções 15 e 16 registram a evolução
> posterior do mesmo checkout. Browser Surface nativa, perfil WebView2 por
> surface, proxy scoped e policies fail-closed agora existem em source. Os
> testes instalados de isolamento de cookies, cleanup, reconnect e abuso
> cross-workspace continuam gates externos. Quando este texto inicial divergir
> dessas seções, o adendo e
> [`../fork-architecture-and-status.md`](../fork-architecture-and-status.md)
> descrevem o estado atual.

---

## 1. Arquitetura entregue

O runtime mantém quatro responsabilidades separadas:

```text
Intelligent Terminal
  ├─ Chat Pane / wta-master  → ACP e sessão da surface focada
  ├─ wta team                → tasks, ownership e heartbeat
  ├─ wta compute             → targets, placement, leases, jobs e snapshots
  └─ wta-node                → runtime Windows/Linux e ponte remota por stdio
```

As invariantes implementadas são:

1. o Chat Pane segue a surface focada; não possui seletor artificial de
   workspace/surface/team;
2. uma Managed Agent Surface possui `SurfaceBinding`, sessão ACP, HomeTarget,
   worktree e lease próprios;
3. Plain Terminal e Managed Agent Surface são tipos distintos;
4. o HomeTarget é sticky durante a vida da sessão;
5. build/test/lint são jobs explícitos; comandos arbitrários do PTY não são
   interceptados;
6. snapshots são imutáveis e identificados por SHA-256;
7. targets de produção ou restritos nunca entram automaticamente no placement;
8. UI, CLI e Chat Pane leem o mesmo store de compute;
9. ACP não é usado como scheduler;
10. nenhum app-server ou endpoint ACP é publicado na rede.

---

## 2. RC-P0 — contratos e store

Implementado em `tools/wta/src/compute/`:

- ADR-009 a ADR-014 e `doc/compute-capability-map.md`;
- `ComputeTarget`, `ComputePolicy`, `SurfaceBinding`, `PlacementDecision`;
- `ExecutionRequest`, `ExecutionJob`, `Snapshot`, `Lease`, `ComputeEvent`;
- IDs estáveis e campos de provider, OS, arquitetura, capabilities, trust,
  allowlist, slots, saúde, latência, fila e custo;
- store versionado em
  `%LOCALAPPDATA%\IntelligentTerminal\compute\v1`;
- gravação atômica de targets, policies, bindings, leases, jobs, snapshots e
  eventos;
- lookup por `workspace_id + pane_id + surface_id`, usado pelo foco real da UI.

O store permanece em arquivos JSON porque o volume e a concorrência atuais
não justificam introduzir SQLite. A fronteira de persistência está encapsulada,
permitindo essa migração futuramente sem alterar o modelo público.

---

## 3. RC-P1 — OpenSSH Provider

Implementado:

- leitura recursiva de `Include`, com limite de profundidade, detecção de ciclo,
  glob, caminhos relativos e `~`;
- importação somente de aliases concretos de `Host`;
- rejeição de wildcard e caracteres capazes de injetar opções;
- resolução efetiva com `ssh -G`;
- preservação da precedência do OpenSSH;
- probe de saúde/plataforma;
- criação fail-closed: todo target SSH descoberto nasce `restricted`,
  `disabled` e fora do placement automático.

O terminal e o CLI utilizam o alias resolvido como argumento fixo de
`ssh.exe`; não constroem uma linha de shell concatenada.

---

## 4. RC-P2 — runtime `wta-node`

Entregue como segundo binário do package WTA:

- `wta-node.exe` para Windows;
- `tools/wta/remote/linux-x64/wta-node` para Linux x86_64;
- `node.handshake`, `node.status`, `node.doctor`, `node.exec` e ponte
  `node acp`;
- JSON-RPC por stdio;
- relatório de versão, OS, arquitetura e capabilities;
- verificação SHA-256 antes do bootstrap/uso remoto;
- daemon por usuário com socket Unix `0600`, diretórios `0700` e ownership do
  processo ACP fora do ciclo de vida do canal SSH;
- `acp start`, `attach`, `detach`, `stop` e `list`, com uma identidade
  `remote_session_id` estável por surface;
- virtualização de IDs JSON-RPC: cada attach pode reutilizar os IDs do cliente
  sem colidir com requests anteriores do adapter persistente;
- cache da resposta ACP `initialize` e replay com o ID do novo cliente durante
  reattach, sem reinicializar o processo upstream;
- framing ACP por linha, em vez de encaminhar chunks arbitrários de pipe;
- registry atômico de sessões sem persistir argv potencialmente sensível;
- backlog limitado e entrega at-least-once durante quebra/reconexão;
- upgrade governado do daemon: o SHA-256 do executável em disco é comparado ao
  digest do processo; uma troca de binário encerra adapters, remove o socket e
  deixa o retry do master iniciar a versão nova;
- processo e logs estruturados sem endpoint de rede público.

SHA-256 observado do helper Linux empacotado:

```text
3939A784A85F77FD41612E87F27B5583618B43762012E117ACF496209466F9A7
```

---

## 5. RC-P3 — Managed Agent Surface

Implementado:

- criação por `New managed agent surface` no dropdown da surface;
- seleção independente de target e adapter ACP;
- targets local, WSL e SSH passam pelo mesmo contrato;
- adapters Codex/Claude no WSL dependem do launcher `npx`, não de uma cópia
  separada do CLI no `PATH`;
- runtime Node.js Linux privado em
  `~/.local/share/intelligent-terminal/toolchains/node-current`, provisionado
  com archive oficial e SHA-256 fixado, sem modificar o profile global;
- bootstrap do node remoto com hash esperado;
- resolução de `AgentSource` para host local, distribuição WSL ou target SSH;
- uma instância `SurfaceAgentRuntime` por surface;
- `remote_session_id` derivado do GUID da surface e propagado por
  UI → helper → `_meta.wta` → `wta-master` → `wta-node`;
- promoção do binding provisório para `managed_agent`;
- observação de foco cria somente `plain_terminal`; promoção para managed exige
  target, agent e adapter explícitos;
- Chat Pane e histórico ligados à sessão ACP da surface focada;
- troca de foco troca a sessão exibida sem misturar transcript;
- queda de transporte SSH remove somente o adapter afetado do pool local,
  identifica seus helpers e solicita rewarm das surfaces no mesmo runtime;
- fallback explícito quando não existe agente gerenciado.

O dropdown da direita também cria surfaces de terminal heterogêneas a partir
dos perfis nativos do Windows Terminal. O clique primário em `+` continua sendo
o fast path que duplica profile/cwd; a seta abre o seletor completo.

---

## 6. RC-P4 — sticky placement

Implementado:

- registry de targets;
- constraints de OS, arquitetura, capabilities, memória, slots, trust,
  allowlist de projeto, credenciais e estado;
- scoring determinístico por afinidade, saúde, latência, carga, cache, custo e
  anti-afinidade;
- decisão explicável com razões de inclusão/exclusão;
- lease por binding;
- pin manual;
- política `Sticky Auto`: escolher uma vez, manter HomeTarget e reconectar ao
  mesmo runtime;
- backoff remoto limitado de 3, 6, 12 e 24 segundos para falhas transitórias,
  sem retry de autenticação, binário ausente ou política;
- produção/restricted desabilitados para seleção automática.

O CLI expõe target discovery/list/show/probe/enable/disable, policies,
placement, bindings, sessions e leases.

---

## 7. RC-P5 — routed execution

Implementado:

- `wta compute exec --class ... --target ... -- <argv>`;
- execução explícita local, WSL ou SSH;
- `ExecutionRequest` persistido antes do start;
- logs e transições de estado estruturados;
- timeout e cancelamento;
- manifesto de artefatos;
- retries somente quando o request é idempotente;
- jobs destrutivos sem retry automático;
- UI “Routed execution” lendo os mesmos jobs do CLI.

O PTY não é interceptado. Digitar `npm test`, pipelines PowerShell ou comandos
interativos continua executando no terminal escolhido pelo usuário.

---

## 8. RC-P6 — snapshots e handoff

Implementado:

- `GitReplica` para estado baseado em commit/branch;
- snapshot de dirty tree com tracked, untracked permitidos, deletes e
  permissões;
- exclusões de secrets e arquivos não declarados;
- digest SHA-256 e materialização segura;
- um writer por worktree;
- preview/apply de handoff;
- generation check antes da aplicação;
- rollback e revogação de lease em falha;
- transferência de sessão por handoff explícito, nunca migração silenciosa de
  um processo vivo.

---

## 9. RC-P7 — elastic compute

Implementado como primitives fail-closed:

- política de orçamento e quota;
- comandos explícitos de start/deallocate;
- idle shutdown configurável;
- nenhuma VM de produção descoberta é promovida ao pool genérico;
- nenhuma mutação Azure ocorre durante discovery, build, teste ou instalação.

Acionar start/deallocate continua sendo uma operação opt-in do usuário. A
validação real dessa fase requer uma VM de desenvolvimento isolada e uma
assinatura autorizada.

---

## 10. RC-P8 — operação avançada

Implementado:

- `wta compute events`;
- `wta compute top`;
- status de targets, leases, bindings, sessões e jobs;
- Agents & Tasks com cards de compute targets e routed execution;
- probe/enable/disable e criação de Managed Codex Surface na UI;
- health, slots e decisões observáveis;
- eventos de placement, execução, handoff e lease persistidos;
- equivalência UI/CLI para as ações entregues.

CAS/chunk deduplication permanece deliberadamente adiado: snapshots integrais
ainda não demonstraram volume suficiente para justificar esse subsistema.

---

## 11. Empacotamento e instalação observados

Foi produzido e instalado:

```text
C:\Users\David\Documents\intelligent-terminal\artifacts\local-installer\
intelligent-terminal-0.9.4.12-x64-release-setup.exe
```

Diretório instalado:

```text
C:\Users\David\AppData\Local\Programs\IntelligentTerminal
```

Evidência do pacote e da instalação Release:

```text
setup SHA-256:
9C0E30154F2E6255D1A503C42110E5FCE50125E601B22175F6FB8458AB99D126

WindowsTerminal.exe SHA-256:
990B682A289CD512F48B58B58144C7505915DBF38C9B8807A74DE7264A84A28C

wta.exe SHA-256:
E2B040B3E41E4449CF4499D71F4B5EC73FD8E29D05A31ADEE952823227D58352

wta-node.exe SHA-256:
AA8524D68F20A6CDAF97E27B33F2634C3D4E43227F5D3AD006816335D52B15DE

remote/linux-x64/wta-node SHA-256:
A5DB38B244D43387E0B425EBA98D42AA188C8CDDB03351C99CA55E479101818E

intelligent-terminal-0.9.4.12-x64-release-setup.exe SHA-256:
3BA7DE92C16464F359A011139FCE7C5CA0A6995A6928159993384E101B0A16FA
```

O instalador terminou com código `0`. O handshake interno autenticado confirmou
`connected=true` e protocolo `3.1`. O executável instalado foi iniciado pelo caminho
`C:\Users\David\AppData\Local\Programs\IntelligentTerminal\WindowsTerminal.exe`
e foi observado em execução sem substituir nem encerrar o Windows Terminal
original. A distribuição unpackaged não possui um launcher
separado chamado `IntelligentTerminal.exe`; esse nome usado em uma versão
anterior do relatório estava incorreto.

O pacote inclui:

- `WindowsTerminal.exe`;
- `wtcli.exe` do mesmo build;
- `wta.exe`;
- `wta-node.exe`;
- `wta-node-linux-x64` (gerado de
  `tools\wta\remote\linux-x64\wta-node`);
- `protocol-version.json`;
- metadados e hashes do build.

Uma inspeção visual anterior do mesmo pacote `0.9.4.12`, em configuração
Debug, confirmou:

- sidebar opaca sem gutter transparente;
- workspace e surface strip separados;
- dropdown da direita aberto com perfis heterogêneos, SSH e
  `New managed agent surface`;
- Agents & Tasks exibindo o target local saudável e SSH como
  restricted/disabled;
- status “Agent mesh · no managed agents” coerente quando nenhuma Managed
  Agent Surface está ativa.

Capturas locais:

- `%TEMP%\intelligent-terminal-surface-profile-menu.png`;
- `%TEMP%\intelligent-terminal-agents-tasks.png`.

A configuração Release atual foi validada por processo, hashes, testes,
verificadores estáticos e build nativo. Ela não foi reclassificada como
“visualmente observada” nesta execução porque a automação disponível proíbe
controlar aplicativos de terminal.

---

## 12. Verificação executada

### Rust

```text
cargo test --manifest-path tools/wta/Cargo.toml
32 library tests passed
1226 binary tests passed
1278 total passed, 0 failed
```

O build Windows exigido também concluiu:

```text
cargo build --target x86_64-pc-windows-msvc \
  --manifest-path tools/wta/Cargo.toml
```

O Rust 1.93 instalado apresentou um ICE no cache incremental
(`evaluate_obligation`). A validação e os builds Release foram repetidos com
`CARGO_INCREMENTAL=0` e passaram. O toolchain global do usuário não foi
alterado.

### Linux/WSL

```text
cargo test --locked --lib --manifest-path tools/wta/Cargo.toml
20 passed, 0 failed
```

Os sources e o `CARGO_TARGET_DIR` permaneceram no ext4 do WSL; o build não
executou Cargo sobre `/mnt/c`. O vigésimo teste abre duas sessões persistentes
independentes, confirma PIDs distintos e verifica bidirecionalmente que o
stream de uma surface não aparece na outra.

O inventário de terceiros foi regenerado após as novas dependencies:

- `tools/wta/cgmanifest.json`;
- `tools/wta/NOTICE.md`;
- 276 crates processados.

### C++/XAML

```text
MSBuild OpenConsole.slnx /t:Build /m:1 /p:CL_MPCount=1
0 errors
```

O primeiro build irrestrito esgotou a memória do PCH (`C3859/C1076`). A
reexecução serial documentada concluiu sem erros; isso confirma que a falha era
pressão de memória do host, não diagnóstico de fonte.

O builder Windows efêmero também executou o grafo x64 Release completo no run
`run-20260729-012330-5f5588cc`. O par WTA exato foi compilado antes do MSIX,
os quatro estágios MSBuild terminaram com zero erros e o setup final
`0.9.4.12` foi capturado com SHA-256
`44b6025c9eb4ec22ad0e80eb2dc506652eec90b36c167a7f495f2a4094cfa447`.
O manifesto e os seis payloads de evidência passaram `Complete` e
`Verify -VerifyCurrentSource`; a VM terminou `PowerState/deallocated`.
Esse run atesta o dirty worktree identificado pelo fingerprint
`4b1f4be4f0e92106cb1c2ba46a7c8c0c97b5e8e9b85deaec6e70bb8d208c2e1d`,
mas não substitui o gate de release assinado a partir de fonte limpa.

### Contratos estáticos

Passaram:

- `Verify-NativeChatDock.ps1`;
- `Verify-WorkspaceNavigation.ps1`;
- `Verify-TerminalProtocolSecurity.ps1`;
- `Verify-IntelligentTerminalVersion.ps1`.

Foi observado:

- protocolo `3.1`;
- produto `0.9.4`;
- pacote `0.9.4.12`;
- 18 métodos do Terminal Protocol protegidos por capability;
- Chat Dock nativo XAML, sem WebView2;
- roteamento por surface focada e ausência do seletor manual de escopo.

### Runtime

Passaram:

- `wta-node status` no Windows instalado;
- handshake JSON-RPC do node Windows;
- execução do helper Linux no Ubuntu 22.04/WSL;
- handshake Linux com `os=linux`, `arch=x86_64` e capabilities
  `exec`, `sha256`, `resource_probe`, `session_registry` e
  `acp_reattach_v1`;
- probe real de `npx -y @agentclientprotocol/codex-acp@1.1.7` no Windows,
  incluindo initialize, `session/list` e capabilities anunciadas;
- discovery WSL real de Codex e Claude via o `npx` Linux privado;
- E2E real no Ubuntu 22.04/WSL com duas instâncias independentes de
  `@agentclientprotocol/codex-acp@1.1.7`, iniciado com
  `Test-WtaNodePersistentAcp.ps1 -VerifyIsolation`: as duas fizeram initialize
  e o primeiro `session/list`; os PIDs `1010` e `1214` eram distintos e cada
  PID permaneceu estável após detach/attach, sem compartilhamento de processo
  entre as duas surface sessions;
- o mesmo E2E executou uma operação autenticada depois do reattach e registrou
  de forma estruturada `reattach_session_list_ok=false` e
  `authentication_gate_detected=true` para ambas as sessões, com a resposta
  exata `Authentication required`; o modo
  `-RequireAuthenticatedReattach` transforma esse gate em falha obrigatória
  quando a credencial WSL tiver sido renovada;
- essa repetição usou diretamente o `wta.exe` e o
  `wta-node-linux-x64` instalados e limpou as duas sessões ao terminar;
- teste do registry confirmou ausência do argv e de marcador secreto;
- teste de upgrade confirmou que mudança do hash do daemon é detectada e causa
  shutdown governado antes do restart;
- build Linux Release executado no filesystem ext4 do WSL pelo wrapper
  `Build-WtaNodeLinux.ps1`;
- discovery persistido de targets;
- `wta compute top` no binário instalado leu o store canônico e reportou
  13 targets, 2 Managed Agents históricos, 0 leases e 0 jobs; os bindings
  criados pelos E2Es falhos desta revisão foram removidos por ID exato;
- harness físico restrito a `do-codex`/`codex-agent`: trust explícito,
  enable e bootstrap do node Linux x64, com versão `0.9.4` e SHA-256
  `a5db38b244d43387e0b425eba98d42aa188c8cddb03351c99ca55e479101818e`;
- PTY físico persistente: session ID `physical-pty-6870f17b2a434194`,
  PID `2227784` antes e depois da interrupção forçada do transporte, com
  backlog recuperado;
- duas sessões Codex ACP físicas e autenticadas: PIDs `2228059` e `2228718`,
  ambos preservados após reattach e com isolamento de processo/stream;
- upload SSH físico `transfer-5c55cdcb-6392-49aa-b198-104184f0d49a` e
  download `transfer-880e514f-50cf-4773-a92e-d0427da82ab2` foram concluídos
  com o mesmo SHA-256
  `3503a708568a5bb3aa67ff96793c497f28526294e7ec96236eb870110c513eb1`;
- a matriz física `transfer-matrix-70de7cee4fb444fea2850b6176d44fae`
  aprovou 0 B, nome Unicode e 1 MiB; o cancelamento de uma transferência de
  512 MiB terminou em `cancelled` e removeu o temporário remoto;
- o proxy físico `proxy-e2e-e5caccfb5e5d472eb0f54ee2274fcde8`
  permaneceu ligado apenas em `127.0.0.1`, atravessou HTTP, HTTPS, WebSocket e
  um serviço remoto localhost-only; crash do supervisor encerrou o SSH exato,
  fechou a porta e foi reconciliado como falha;
- o relay WSL
  `relay-workspace-e21bf59d761d48218f21d5dbf8b331e0` preservou o journal
  entre attachments e rejeitou nonce repetido, cross-surface e uso após
  revogação;
- o bootstrap remoto compara o SHA-256 do binário versionado antes de substituir
  o inode, e o cliente faz uma única nova tentativa quando o daemon fecha antes
  da primeira resposta durante rollover intencional;
- o Compute Store normaliza GUIDs entre WinRT/COM, remove bindings por identidade
  de surface de forma idempotente, revoga seus leases e recupera imediatamente
  um `state.lock` cujo PID WTA já terminou;
- o E2E instalado confirmou protocolo `3.1`, surfaces `1 → 2 → 3 → 4 → 1`,
  perfil Command Prompt heterogêneo, remoção do binding gerenciado e liberação
  de todos os leases;
- `compute doctor ssh`, `doctor surface`, `doctor agent`, reconcile e stop
  exatos usam o mesmo Compute Store; o bundle redigido do harness físico
  passou a inspeção contra credentials, source paths, environment e secrets;
- inspeção visual da versão instalada.

---

## 13. Gates externos ainda não observados

Os seguintes gates continuam dependentes de infraestrutura, UX interativa ou
política de segurança e não foram substituídos por mocks:

1. alteração real de host key e confirmação de falha fechada;
2. jitter, packet loss prolongado, suspensão do cliente e TUI longa;
3. duas Managed Agent Surfaces criadas e alternadas pela UI durante streaming
   no host físico (o isolamento e reattach dos runtimes já foram observados);
4. adapters Claude e Gemini autenticados no host físico;
5. cancelamento sob perda/jitter de rede;
6. relay remoto-local pela UI física e teste negativo cross-workspace
   end-to-end (as primitivas capability-scoped já passaram em WSL);
7. Browser Surface instalada, isolamento de WebView2 e cleanup por workspace;
8. session restore do aplicativo retomando os runtimes físicos;
9. custo observado por ciclo e política de idle shutdown da VM Azure
   (start/deallocate fail-closed já é exercitado pelo builder);
10. benchmark que justifique ou refute CAS/chunk dedupe.

A autenticação Codex no devbox físico passou nas duas sessões do harness. O
resultado histórico `Authentication required` permanece relevante somente para
o ambiente WSL antigo; não representa o estado observado em `do-codex`.

Esses itens são gates de aceitação externa, não código omitido. Devem ser
executados quando existir um devbox seguro e autorizado; produção não deve ser
usada para “completar” a validação.

---

## 14. Estado final

A implementação WTA está compilada e testada em Windows, WSL e no devbox SSH
físico autorizado. Bootstrap, PTY persistente, reconnect ao mesmo PID, duas
sessões Codex ACP autenticadas/isoladas, transferência/cancelamento em volume,
proxy remoto, diagnóstico e evidência redigida foram observados fisicamente.

Isso ainda não autoriza o rótulo de paridade total com cmux SSH: a Browser
Surface atual é preview, e relay pela UI física, host-key rotation, matriz
multi-adapter, restore pela UI, isolamento/cleanup cross-workspace e hardening
prolongado continuam gates explícitos. Produção não será usada para fechar
qualquer gate.

---

## 15. Vertical slice Environment + Files + Browser — 2026-07-29

### Implementado

- `ExecutionEnvironment`, `LaunchMethod`, `AccessEndpoint` e o supervisor único
  são persistidos pelo Compute Store.
- Workspaces, bindings, proxies e browsers referenciam environment/endpoint
  estáveis. SSH continua bootstrap/fallback; endpoint público permanece
  desabilitado fail-closed.
- Node, proxy e file clients pedem permits ao mesmo supervisor.
- Restore captura referências estáveis e planeja `ReconnectEnvironment`;
  testes rejeitam portas, PIDs, túneis e autenticação no snapshot.
- `RemoteFileRootPolicy` vincula root opaco a workspace, target e binding.
  Read/write/delete são capabilities independentes. HOME/admin exige
  reconhecimento explícito e `files.admin_roots`.
- `wta-node` não expõe mais File Explorer nem preparação de download por raw
  path. `compute transfer download` antigo falha fechado. O fluxo funcional
  prepara download por `file.prepare_download` após autorização do root.
- Browser Surface usa Terminal Protocol 3.1 `CreateSurface`, resolve o pane
  exato e aplica perfil/proxy/policy WebView2 isolados.
- `Agents & Tasks` lê environments, conexões, roots redigidos e RSS/CPU do
  workspace context canônico.

### Verificação observada

| Gate | Resultado |
|---|---|
| `cargo +stable test --lib` | 70 passed, 0 failed |
| `cargo +stable check --all-targets` | passou; warnings preexistentes |
| `Verify-RemoteRuntimeVerticalSlice.ps1` | passou |
| `Verify-TerminalProtocolSecurity.ps1` | passou; protocolo 3.1, 18 métodos guardados |
| build Azure WTA release anterior | passou |
| build Azure Cascadia anterior | falhou no primeiro compile de BrowserPaneContent |

A falha Cascadia anterior foi determinística: `Microsoft::WRL` foi resolvido
como `winrt::Microsoft`, os revokers usaram o namespace XAML errado e faltou
`WtExeUtils.h`. Essas causas foram corrigidas no source. Um build Azure do
snapshot atual ainda é obrigatório antes de instalar esta revisão.

### Gates abertos

- package/installer Azure do source atual;
- criação e foco de Browser/File Explorer/environment na UI instalada;
- cookie/profile isolation e cleanup cross-workspace;
- restart do app retomando o mesmo runtime físico;
- interrupção SSH durante browser/file streaming;
- host-key rotation e matriz prolongada de falhas de rede.

Nenhum mock, unit test ou verificador de source é reportado como evidência
desses gates.

---

## 16. Fechamento do build, instalador e root-scoped SSH — 2026-07-30

Esta seção substitui o status transitório do final da seção 15.

### Build e proveniência

- O snapshot completo foi compilado na VM Azure dedicada no run
  `run-20260729-204931-9b422ea8`, source fingerprint
  `bea7ed4b14d95cd63cf7d07d0710d678dd68b87f41cc423d67cfeadcaef0d421`.
- O instalador-base produzido pela VM tem SHA-256
  `46564aeb83f0c200cc2e599a3d5ad0a40542063261c1a8ec921f107f328f139d`.
- O hotfix exclusivamente do script de instalação foi reempacotado sem
  recompilar ou alterar `payload.zip`. O manifest de proveniência registra o
  hash do bootstrap, do payload original, de cada script substituído e do
  instalador final.
- `TerminalApp.dll` instalado foi inspecionado com `dumpbin /dependents`: não
  importa `WebView2Loader.dll`, e nenhum loader dinâmico foi enviado no root.
  O loader WebView2 é ligado estaticamente.
- A VM `vm-intelligent-terminal-build-01` foi observada em
  `VM deallocated`, com provisioning state `Succeeded`.

### Upgrade seguro

- O COM proxy/stub agora é instalado em
  `proxies/<sha256>/OpenConsoleProxy.dll` e o registro per-user aponta para
  esse caminho imutável. O instalador nunca sobrescreve um proxy ainda
  mapeado por outro processo.
- Um upgrade `/quiet` foi executado enquanto o Intelligent Terminal estava
  aberto. O instalador encerrou com código 0.
- `settings.json` e `state.json` mantiveram exatamente os hashes anteriores
  após instalação limpa, falha intermediária e upgrade com o aplicativo
  aberto.
- O protocolo instalado respondeu `connected=true`, versão `3.1`.
- O E2E instalado criou surface comum, duplicata e Managed Agent Surface,
  confirmou o binding Codex e retornou a topologia de `1 → 2 → 3 → 4 → 1`.
  O cleanup removeu o binding gerenciado e liberou os leases.

### SSH físico com File Explorer fail-closed

- O harness físico deixou de usar o download legado por caminho absoluto.
  Esse caminho foi observado falhando fechado com a mensagem de que downloads
  remotos sem escopo estão desabilitados.
- O fluxo aprovado cria uma `RemoteFileRootPolicy` temporária vinculada a
  target/workspace, usa somente `root_id + relative_path` no download e revoga
  a policy no cleanup.
- No `do-codex`, o helper físico observado tem SHA-256
  `1906e53f24e2431ed92bc4d195f93090c5a610bac8cd41b66dac109a11b49a33`.
- O PTY `physical-pty-e20452ac9e3145ff` preservou o PID `2277973` após perda
  do transporte e reattach.
- Duas sessões Codex ACP autenticadas e isoladas usaram PIDs `2278247` e
  `2278868`.
- Upload e download root-scoped preservaram o SHA-256
  `4fc2ec4f98c5d8d253a1c8232bd51837e5a400c29ee17c9725795a1590e1f65d`.
- A matriz física adicional passou para arquivo vazio, nome Unicode e 16 MiB.
- Host key incorreta em known_hosts isolado, aliases curinga e injeção de
  opções foram rejeitados sem modificar o known_hosts real.

### Regressão final

| Gate | Resultado observado |
|---|---|
| `cargo +stable test --manifest-path tools/wta/Cargo.toml` | 70 testes da lib + 1226 testes do binário; 0 falhas |
| verificadores runtime/protocolo/workspace/chat/installer | todos passaram |
| `git diff --check` | passou |
| inspeção UI Automation | janela viva; sidebar, workspaces, Agents and Teams, new-surface split button, Agent Mesh e histórico presentes |

### Gates que permanecem externos

O fechamento acima não muda os gates deliberadamente fail-closed da seção 13:
rotação real da host key, rede degradada prolongada, alternância física durante
streaming, matriz Claude/Gemini, relay pela UI, restore físico pelo aplicativo
e browser WebView2 com isolamento cross-workspace. Eles não são substituídos
por testes unitários nem pelo E2E Codex/SSH já aprovado.
