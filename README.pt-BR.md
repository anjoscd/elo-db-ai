# ELO-DB-AI

Motor local de contexto e memória para agentes de código, escrito em Rust. Funciona em qualquer IDE e com qualquer modelo: servidor MCP e adaptadores de ganchos para Claude Code, Codex, Gemini CLI, Cursor e GitHub Copilot.

[English](README.md) · [Especificação](docs/SPEC.md) · [Avaliação](docs/EVALUATION.md) · [Pesquisa](docs/RESEARCH.md) · [Histórico de alterações](CHANGELOG.md)

## Por que o ELO

**Por que usamos.** Um agente só segue a regra que ele enxerga na hora em que age. Colar todas as regras em todo pedido desperdiça contexto e esconde a que importa; deixar as regras de fora faz o agente repetir erros antigos. O ELO manda as poucas regras e decisões que valem para *este* pedido, *este* projeto e *este* arquivo, e lista o resto como ponteiros curtos. Num teste às cegas com pedidos reais, ele venceu a injeção de todas as regras em 74% dos casos decididos, enviando cerca de um quarto dos bytes (≈1,4 KB contra ≈5,4 KB por pedido).

**Tecnologias.**

| Parte | Tecnologia |
|---|---|
| Motor | Rust (um binário só, sem runtime) |
| Memória | SQLite com busca de texto FTS5 |
| Mapa de código | tree-sitter (quem chama quem, importações em JavaScript/TypeScript) |
| Integração | servidor MCP (stdio) + adaptadores de ganchos para Claude Code, Codex, Gemini CLI, Cursor, GitHub Copilot |
| Opcional | embeddings estáticos locais (formato model2vec), desligados por padrão |

**Por que deixa a programação mais segura.**

- **Nada sai da sua máquina.** Sem chamadas de rede e sem telemetria; memória, índices e registros ficam em `.elo/`. A única exceção é o comando manual `elo update` (a partir da 0.2.0), que só roda quando você digita.
- **A regra certa antes do passo arriscado.** Regras presas a arquivos (migrations, scripts de publicação, configuração) chegam logo antes de o agente ler ou editar esses arquivos.
- **Travas em comandos perigosos.** Travas configuráveis registram, pedem confirmação ou barram a chamada (ex.: push forçado, publicação, apagar).
- **Só você escreve as leis.** Leis exigem `--as-user`; o agente não cria nem promove leis sozinho.
- **Sem contexto velho ou misturado.** Decisões superadas deixam de ser entregues, e itens de outro projeto chegam só como ponteiro.
- **Segredos fora dos registros.** Chaves e tokens são removidos dos pedidos antes de qualquer gravação.
- **Auditável.** `elo explain` e `elo stats` mostram por que cada item foi enviado e se foi usado.

## O problema

Agentes de código esquecem regras e decisões entre conversas. Carregar toda a memória em cada pedido consome contexto e pode trazer informações desatualizadas ou sem relação com o trabalho. Um sistema de memória útil precisa entregar a regra certa quando o agente está prestes a precisar dela, mantendo o restante disponível para consulta.

## O que o ELO faz

- **Memória persistente:** armazena itens `law`, `rule`, `decision`, `note` e `pitfall` em SQLite, com gatilhos por palavra e caminho, importância, histórico e escopos (`global`, `project:<nome>`, `link:<nome>`). Oferece busca, importação JSONL, substituição, expiração e arquivamento.
- **Contexto no pedido:** seleciona itens vigentes por escopo, gatilhos e pontuação, com orçamento de bytes, cotas por camada, deduplicação por sessão e ponteiros para contexto adicional. Notas e decisões têm limites próprios e aparecem como ponteiros por padrão, salvo quando sinais mais fortes justificam o texto completo.
- **Ganchos no momento do trabalho:** entrega contexto no pedido, um resumo do trabalho no início da sessão e regras presas ao arquivo antes de chamadas de ferramentas. Travas configuráveis podem registrar correspondências, pedir confirmação ou bloquear uma chamada.
- **Placar local de utilidade:** registra entregas, bytes, feedback manual e evidências automáticas de chamadas posteriores que tocam arquivos ou identificadores correspondentes. Isso é um sinal de uso, não uma prova de que a regra foi seguida ou a tarefa deu certo.
- **Mapa de código:** indexa incrementalmente JavaScript e TypeScript (`.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.tsx`) com tree-sitter. Registra entidades, importações, chamadas, renderização JSX, callbacks e reexportações; consulta quem chama quem, importadores e impacto transitivo. Relatórios de referências distinguem resultados resolvidos e não resolvidos e informam arquivos desatualizados.
- **Quadro de trabalho:** acompanha projetos, tarefas, dependências, marcos e passagens entre conversas. Trabalho aberto sem atividade pode aparecer como adiado.
- **Receitas e manutenção:** guarda procedimentos reutilizáveis como notas com gatilhos; propõe consolidação da memória e aplica as alterações suportadas com backup.
- **Avaliação:** executa um benchmark local com rótulos e reproduz logs locais de pedidos com saída agregada.

## Princípios

- **Armazenamento e processamento locais.** O ELO não faz chamadas de rede nem tem telemetria. Memória, índices, estado de sessão e logs ficam em `.elo/`. A saída dos ganchos é entregue ao Claude Code; o tratamento desse contexto pelo aplicativo é separado do processamento local do ELO.
- **Orçamentos explícitos de contexto.** A seleção por pedido usa `workspace.budget_bytes` em `elo.toml`; notas/decisões, regras por arquivo e resumos de sessão também têm limites. Bytes medem o tamanho da saída, não o consumo exato de tokens.
- **Leis sob controle do usuário.** Criar ou importar leis exige `--as-user`; a promoção aceita apenas itens de origem do usuário. A gravação de leis valida limites de tamanho e orçamento. A manutenção informa leis duplicadas para revisão manual.
- **Comportamento verificável.** `elo explain`, `elo stats` e logs locais mostram seleção e uso. Referências de código não resolvidas permanecem visíveis.

## Instalação

O ELO é um programa em Rust. Instale o Rust (que já traz o Cargo) pelo [rustup.rs](https://rustup.rs) e depois instale o ELO:

```sh
# direto do GitHub
cargo install --git https://github.com/anjoscd/elo-db-ai --locked

# ou de uma cópia local
cargo install --path . --locked
```

Isso coloca o `elo` em `~/.cargo/bin`, que o rustup já põe no seu `PATH`. Na pasta de trabalho que deseja gerenciar, execute:

```sh
elo init
elo doctor
```

`elo init` cria `.elo/` e um `elo.toml` com padrões e projetos descobertos, preservando uma configuração existente. Execute os comandos da pasta de trabalho na raiz dela. Os ganchos localizam essa raiz pelo `cwd` do evento.

## Atualização

O ELO nunca se atualiza sozinho, executa atualizações em ganchos ou dispara atualizações pelo servidor MCP. Para atualizar o binário `elo` manualmente:

```sh
# consulta a versão atual e a mais nova sem baixar nada
elo update --check

# instala o release mais recente para a sua plataforma
elo update

# instala um release específico
elo update --version 0.2.0
```

O comando usa o `curl` do sistema (com `--fail --location --proto =https --tlsv1.2`) para baixar o arquivo da release correspondente e o seu `SHA256SUMS`, verifica o digest SHA-256, extrai o binário `elo` e substitui atomicamente o binário atual, mantendo o anterior como `elo.old`.

## Usar em qualquer IDE (MCP)

O servidor fala MCP 2026-07-28 e as versões legadas 2025-11-25, 2025-06-18, 2025-03-26 e 2024-11-05.

Execute `elo mcp` para expor o ELO por MCP stdio a clientes compatíveis, independentemente do modelo. Primeiro execute `elo init` no workspace e coloque `elo` no `PATH` do cliente. O servidor encontra o ancestral mais próximo com `.elo/`; use `--root` quando o cliente iniciar em outro diretório. Mescle a configuração abaixo às opções existentes e substitua `/path/to/workspace` pelo caminho absoluto do workspace.

**VS Code — `.vscode/mcp.json`**

```json
{
  "servers": {
    "elo": {
      "type": "stdio",
      "command": "elo",
      "args": ["mcp", "--root", "/path/to/workspace"]
    }
  }
}
```

**Cursor — `.cursor/mcp.json`**

```json
{
  "mcpServers": {
    "elo": {
      "command": "elo",
      "args": ["mcp", "--root", "/path/to/workspace"]
    }
  }
}
```

**Codex CLI — `~/.codex/config.toml`**

```toml
[mcp_servers.elo]
command = "elo"
args = ["mcp", "--root", "/path/to/workspace"]
```

**Gemini CLI — `.gemini/settings.json`**

```json
{
  "mcpServers": {
    "elo": {
      "command": "elo",
      "args": ["mcp", "--root", "/path/to/workspace"]
    }
  }
}
```

Ferramentas: `elo_recall` busca memórias; `elo_context` lê um escopo ou assunto; `elo_add` grava texto (tipo padrão `note`, escopo `global`, `keywords` e `paths` opcionais); `elo_explain` explica um item; `elo_map` consulta o mapa de código; `elo_feedback` permite ao agente informar quais ids entregues ele usou ou ignorou (modo sombra: só registra, nunca muda a seleção; só aceita ids entregues na mesma sessão); `elo_maintain_catalog` e `elo_maintain_propose` permitem ao agente sugerir manutenção (caminhos, "sempre", escopo, junções), validada e pendente até você revisar. As gravações têm origem de agente, portanto leis são recusadas. Execute `elo map sync` antes de consultar o mapa. Outros clientes MCP stdio usam o mesmo comando e argumentos; a disponibilidade das ferramentas também depende do suporte MCP do cliente.

### Ganchos do Claude Code

Mescle este bloco `hooks` em `~/.claude/settings.json`, preservando as configurações e os ganchos existentes:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "elo hook user-prompt-submit --live" }] }
    ],
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "elo hook session-start --live" }] }
    ],
    "PreToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "elo hook pre-tool-use --live" }] }
    ],
    "PostToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "elo hook post-tool-use --live" }] }
    ]
  }
}
```

| Evento | Comportamento |
|---|---|
| `UserPromptSubmit` | Seleciona a memória e retorna contexto; registra a seleção localmente. |
| `SessionStart` | Reinicia o estado de entrega e retorna passagens abertas e tarefas em andamento/bloqueadas do projeto ativo no quadro, quando disponíveis. |
| `PreToolUse` | Verifica travas e entrega itens presos ao arquivo que a ferramenta está prestes a tocar. |
| `PostToolUse` | Atualiza o estado da sessão e as evidências automáticas de uso; não emite contexto. |

Use `--shadow` no lugar de `--live` para processar eventos sem retornar contexto ou decisões de permissão. Sem essas opções, os ganchos não fazem nada. Erros dos ganchos são tratados silenciosamente e encerram com sucesso; travas são verificações lexicais, não um ambiente de isolamento de segurança.

Os logs em `.elo/shadow/log.jsonl` contêm o contexto selecionado e o texto truncado dos pedidos. Pedidos com sinais de credenciais são ocultados por heurística, sem garantia de remover todos os segredos. Mantenha `.elo/` privado e fora do controle de versão.

## Uso

### Memória e consulta

```sh
elo add rule --scope global --id rule-tests \
  --title "Testar alterações" --body "Execute os testes relevantes após mudar o comportamento." \
  --triggers testes,testar
elo add law --scope global --id law-secrets --as-user \
  --title "Proteger segredos" --body "Nunca inclua credenciais em commits." \
  --triggers credenciais,segredos
elo recall "testes"
elo context global testes --budget 3000
elo explain rule-tests
elo explain --last
elo good rule-tests
elo stats --by kind
```

`elo bad <id>` registra feedback negativo. `elo supersede <antigo> <novo>` liga uma decisão ou nota à sua substituta; `elo recall <consulta> --history` inclui itens inativos. `elo forget <id>` arquiva o item e preserva seu registro.

`elo add --triggers` define palavras-chave. Para fornecer gatilhos por caminho ou `always`, importe JSONL. Por exemplo, salve esta linha como `memory.jsonl`:

```json
{"id":"rule-auth","kind":"rule","scope":"global","title":"Verificações de autenticação","body":"Valide as permissões antes de alterar dados da conta.","keywords":["autenticação"],"paths":["src/auth/*"]}
```

```sh
elo import memory.jsonl --dry-run
elo import memory.jsonl
```

A correspondência por caminho aceita caminhos exatos e prefixos terminados em `*`. Palavras-chave com formato de arquivo, como `auth.ts`, também prendem o contexto anterior à ferramenta aos componentes correspondentes do caminho. Configure os escopos de projeto e vínculo em `elo.toml`; use `elo discover` para consultar projetos e worktrees detectados.

### Travas

Adicione uma trava a `elo.toml`, por exemplo:

```toml
[[guard]]
id = "confirm-push"
tool = "Bash"
field = "command"
all = ["git push"]
action = "ask"
message = "Confirme antes de enviar as alterações."
enforce = false
```

As correspondências ficam em `.elo/shadow/guards.jsonl`. Defina `enforce = true` com um gancho `PreToolUse` em modo live para retornar `ask` ou `deny`, conforme `action`. O campo opcional `law` referencia um item de memória na mensagem. As travas são configuradas explicitamente; o ELO não converte leis em texto em verificações automáticas.

### Mapa de código

Execute em uma pasta de trabalho com arquivos de código suportados:

```sh
elo map sync
elo map query --contains authenticate
elo map query --calls authenticate
elo map query --callees authenticate
elo map query --imports src/auth.ts
elo map query --impact authenticate --depth 3 --json
```

Use `elo map sync --target src` para indexar uma subpasta. As consultas leem o índice existente; execute sync novamente para atualizar arquivos desatualizados. A resolução é estática e pode deixar referências dinâmicas sem resolução.

### Projetos, tarefas e passagens

```sh
elo project add demo --path . --name "Demo"
elo milestone add --project demo release "Primeira versão"
elo task add --project demo "Revisar autenticação" --milestone release
elo task list --project demo
elo handoff new --project demo --title "Revisão da autenticação" \
  --body "Permissões revisadas; adicionar testes de regressão na próxima conversa."
elo handoff list --project demo
```

Use os IDs retornados com `elo task update <id> --status in_progress`, `elo task add --project demo "Continuação" --blocked-by <id>` e `elo task done <id>`. Consulte e encerre passagens com `elo handoff show <id>` e `elo handoff close <id>`. O cadastro no quadro é separado da configuração de escopos de memória em `elo.toml`.

### Receitas e manutenção

```sh
elo recipe add "Verificar uma alteração" \
  --step "Execute os testes relevantes." \
  --step "Revise o diff em busca de alterações não intencionais." \
  --keywords testes,revisão
elo recipe list
elo maintain
elo maintain --apply
elo maintain review                  # propostas feitas pelo agente
elo maintain review --apply 3,4      # aplica as escolhidas (lei só com id explícito)
elo maintain review --apply-nonlaw   # aplica todas as pendentes, menos leis
elo shadow calibrate                 # faixas de pontuação x uso observado, para calibrar a abstenção
```

Receitas exigem pelo menos dois passos e são armazenadas como notas globais. As opções acima usam os nomes em inglês; os nomes anteriores em português continuam funcionando como aliases.

`elo maintain` mostra um plano. `--apply` primeiro faz backup do banco, depois tenta substituições válidas de duplicados, marca itens expirados e remove registros automáticos de uso antigos ou órfãos. Decisões/notas semelhantes são sugestões para revisão manual; leis duplicadas são informadas sem consolidação automática.

`elo maintain review` lista as propostas enviadas pelo agente via MCP. Cada uma foi validada no envio e é validada de novo antes de ser aplicada, numa única transação; só gatilhos ou escopo mudam, nunca título ou corpo. `elo shadow calibrate` lê o registro local da sombra, que anota se o seletor teria ficado em silêncio em cada pedido, e mostra, por faixa de pontuação, com que frequência algo foi usado e com que frequência você marcou como ruim.

## Avaliação

A partir do repositório do ELO:

```sh
elo bench --split test
elo bench --split dev --json /tmp/elo-bench.json
```

O benchmark compara o seletor do ELO com nenhuma injeção e com um seletor de referência por palavra-chave, usando eventos rotulados, relógio fixo e ordenação por sessão. Ele informa:

- Taxas de itens proibidos e violações de escopo.
- Precisão de relevância e recuperação dos itens necessários: texto completo, acesso por ponteiros e dentro da sessão.
- Cobertura de leis e vazamento de itens substituídos/expirados.
- Bytes de saída em p50/p95 e acerto ao não injetar contexto.

São medidas de seleção de contexto, não resultados de sucesso em tarefas de código, economia de tokens ou latência. O [método de avaliação](docs/EVALUATION.md) também descreve protocolos e metas mais amplos; nem todos estão implementados em `elo bench`.

Reproduza o log local de pedidos de uma pasta de trabalho com:

```sh
elo bench --replay .elo/shadow/log.jsonl
```

O replay lê a memória/configuração atual e os eventos registrados sem alterar o banco. Informa bytes agregados, itens completos por evento, saídas vazias e registros ausentes/inválidos. Pedidos truncados ou ocultados e eventos de ferramenta ausentes limitam a fidelidade; o replay não tem rótulos de relevância.

## Pesquisa

As [notas de pesquisa](docs/RESEARCH.md) reúnem os trabalhos que orientam o projeto do ELO. A [especificação](docs/SPEC.md) inclui requisitos de projeto e ideias adiadas; este README descreve o comportamento implementado na CLI.

## Contribuição

Abra uma issue com um exemplo reproduzível ou envie um pull request focado. Não inclua memórias privadas nem logs da pasta de trabalho nos relatos. Para alterações de código, adicione testes relevantes e execute:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Para alterações no seletor, compare a saída do benchmark e explique o efeito. As contribuições devem ser originais ou compatíveis com a licença do projeto.

## Licença e forks

[Functional Source License 1.1, ALv2 Future License (FSL-1.1-ALv2)](LICENSE). Em resumo:

- **Uso livre:** pessoas e empresas podem usar, modificar e fazer **fork** do ELO, inclusive no trabalho e dentro dos próprios produtos e projetos.
- **Proibido sem licença do autor:** oferecer o próprio ELO, ou um produto ou serviço que **concorra** com ele (por exemplo vendê-lo ou hospedá-lo como serviço pago). Para uma licença comercial, entre em contato com o autor pelas [issues do GitHub](https://github.com/anjoscd/elo-db-ai/issues).
- **Vira Apache 2.0:** cada versão passa a ser Apache License 2.0 dois anos depois de lançada.
- **Crédito obrigatório:** forks e cópias alteradas precisam manter os arquivos [LICENSE](LICENSE) e [NOTICE](NOTICE), dar o crédito a **ELO-DB-AI, de anjoscd** (https://github.com/anjoscd/elo-db-ai) e informar quais arquivos foram alterados.

## Outras ferramentas de programação

| Ferramenta | Ganchos | MCP (`elo mcp`) | Regras exportadas |
| --- | --- | --- | --- |
| Claude Code | Sim (formato padrão) | Sim | AGENTS.md |
| Codex | Sim | Sim | AGENTS.md |
| Gemini CLI | Sim | Sim | GEMINI.md |
| Cursor | Sim | Sim | .cursor/rules/*.mdc + AGENTS.md |
| Copilot CLI / agente | Sim | Sim | .github/copilot-instructions.md, .github/instructions/*.instructions.md + AGENTS.md |
| VS Code | — | Sim | Formato Copilot |
| Windsurf | — | Sim | .devin/rules/*.md + AGENTS.md |
| Cline | — | — | .clinerules/*.md + AGENTS.md |
| Zed / JetBrains | — | Sim | AGENTS.md (carregar manualmente se necessário) |

```sh
elo setup codex                 # imprime caminhos e trechos de configuração
elo setup cursor --project     # configuração do projeto; não grava arquivos
elo hook BeforeAgent --live --format gemini
elo export --format todos --dry-run
elo export --format cursor --output . --scope project:api
```

`elo hook --format claude|codex|gemini|cursor|copilot` aceita os eventos nativos e os nomes atuais em kebab-case. Eventos desconhecidos terminam sem saída. O Gemini recebe somente JSON; BeforeTool não injeta contexto. beforeSubmitPrompt do Cursor responde `{"continue":true}`; userPromptSubmitted do Copilot fica sem saída. Ambos registram o pedido e adiam seu contexto para postToolUse, mostrando o bloco de trabalho uma vez por projeto na sessão. SessionStart pode mostrá-lo antes. apply_patch do Codex e write_file/replace do Gemini atualizam um mapa existente; o Cursor usa afterFileEdit.

As travas continuam em modo sombra, salvo configuração com `enforce = true`. Negações usam o protocolo da ferramenta; Claude e Codex terminam com código 0 e JSON `hookSpecificOutput.permissionDecision: "deny"`. Ambos preservam `ask`; os demais formatos não o convertem em negação.

`elo setup` imprime os ganchos e a configuração MCP, com o caminho de destino no topo de cada trecho. Mescle os trechos com sua configuração existente. Ganchos do Copilot usam `~/.copilot/hooks/elo.json`, ou `.github/hooks/elo.json` com `--project`; no JetBrains, cole o JSON na tela de configuração MCP. Os trechos MCP pressupõem uma versão do ELO que forneça `elo mcp`; setup não implementa nem inicia o servidor.

`elo export --format agents|cursor|copilot|windsurf|windsurf-legacy|cline|gemini|todos` exporta leis ativas e não expiradas. O padrão é `agents`, a pasta padrão é o diretório atual e `--scope` compara o escopo armazenado exatamente. Leis always/de palavra-chave entram em AGENTS.md (e GEMINI.md no formato Gemini); as condições de palavras-chave permanecem como texto. Leis por caminho usam regras nativas, incluindo `pasta/migrations*` e `pasta/migrations*/**` para prefixos ELO. Os formatos de regras nativas também geram AGENTS.md para palavras-chave; configure sua ferramenta para carregá-lo quando necessário.

Arquivos compartilhados preservam tudo fora de `<!-- elo:begin -->` / `<!-- elo:end -->`. Marcadores incompletos ou duplicados e links simbólicos interrompem a exportação sem alterar arquivos. Arquivos `elo-*` nas pastas de regras selecionadas pertencem ao ELO e podem ser regenerados ou removidos quando obsoletos; use outros nomes para regras próprias. O `elo.mdc` do Cursor também preserva texto fora dos marcadores. Uma exportação com escopo preserva arquivos nativos de leis ativas de outros escopos, mas substitui o bloco compartilhado pelo escopo selecionado. Repetir a exportação não muda o resultado; `--dry-run` lista gravações e remoções sem executá-las.

Windsurf / Devin Desktop: `elo setup windsurf` mostra o MCP global atual em `~/.config/devin/mcp_config.json` e o legado em `~/.codeium/windsurf/mcp_config.json`, inclusive com `--project`. `elo export --format windsurf` grava em `.devin/rules`; use `--format windsurf-legacy` para `.windsurf/rules`. `todos` seleciona apenas o formato atual. Se `.clinerules` já for arquivo, recebe um bloco com marcadores; caso contrário, a exportação usa a pasta `.clinerules/`. A verificação de links limita-se à pasta de saída e seus descendentes, sem verificar ancestrais do sistema.
