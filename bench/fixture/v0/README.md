# Fixture sintético v0 — Lantern

Tudo neste diretório é inventado: nomes, contratos, código, contas e mensagens.
Os hosts `.invalid` não representam serviços reais. Não é necessário instalar
pacotes nem executar o código TypeScript para carregar o fixture.

## História

Lantern é uma loja fictícia. `workspace/` contém quatro projetos não sobrepostos:

- `api/`: recebe pedidos e mantém um ledger de pedidos no servidor.
- `web/`: mostra previews acessíveis de recibos; seu cache local é descartável.
- `billing/`: produz faturas em centavos e mantém um diário de conciliação.
- `services/notify/` (projeto `notify`): oferece entrega sandbox e preview de recibos.

O manifesto da raiz declara os quatro membros. Cada membro tem seu próprio
`package.json` e dois ou três arquivos curtos. `elo.toml` descreve a configuração
do workspace; seus roots são relativos a `workspace/`, não ao diretório v0.

Há exatamente dois vínculos:

- `payments`: `api` + `billing`. A importação em `api/src/checkout.ts:1` usa
  `@lantern/billing`, cujo manifesto exporta `billing/src/invoice.ts`.
  O contrato compartilha centavos inteiros, BRL e chave de idempotência.
- `notifications`: `api` + `web` + `notify`. `api/src/notify.ts:1`,
  `web/src/notify.ts:1` e `services/notify/src/server.ts:1` declaram `NOTIFY_URL`
  com o mesmo endpoint fictício. O serviço oferece `/receipts` e `/preview`.

A armadilha é `database`: em `api` significa ledger persistente, em `web`
significa cache efêmero, e em `billing` significa diário de conciliação.
`R04`, `R07` e `R10` usam a mesma keyword, mas seus conteúdos não são
intercambiáveis. Essa palavra não é keyword nem evidência dos vínculos declarados.

## Itens e histórico

| Kind | Contagem |
| --- | ---: |
| law | 6 |
| rule | 21 |
| decision | 9 |
| note | 18 |
| pitfall | 6 |
| Total | 60 |

| Escopo | Contagem |
| --- | ---: |
| global | 6 |
| project:api | 11 |
| project:web | 8 |
| project:billing | 8 |
| project:notify | 8 |
| link:payments | 10 |
| link:notifications | 9 |

As seis leis têm origem `user`, corpo de no máximo 400 bytes UTF-8 e trigger
`release-check`. Não são always-on: tarefas sem esse trigger podem abster-se.
As três cadeias são `D1 → D2 → D3` (cursor da API), `D4 → D5 → D6`
(reembolso em payments) e `D7 → D8 → D9` (rotas de notifications).
Cada cadeia mantém kind e escopo; somente a última versão está ativa.
`N02` e `N03` estão explicitamente expirados, com prazo anterior ao `now`.
Os caminhos de `R16`, `R17` e `R19` permitem recuperar os contratos em eventos
com arquivos tocados mesmo sem keyword no prompt.

## História dos eventos e regras de rotulagem

Os rótulos foram escritos como cartões de cenário, sem executar um resolvedor,
sem usar scores e sem fazer tuning. `allowed_scopes` inclui `global` e os escopos
legítimos da situação. `required` identifica conteúdo necessário à tarefa;
`forbidden` identifica os enganos específicos do cartão. Todo outro item é
neutro para relevância, ainda que a emissão fora dos escopos permitidos continue
sendo uma violação de escopo. Required e forbidden são disjuntos.

| Tipo | História e rótulos | Eventos | dev | test |
| --- | --- | ---: | ---: | ---: |
| abstention | Pedidos pessoais e transformação literal de texto na raiz; apenas global permitido, required vazio; notas expiradas e falsos gatilhos em código proibidos. | 20 | 12 | 8 |
| explicit_project | Na raiz, nomeia web para acessibilidade (R08) ou billing para arredondamento (R11, P3). O segundo cartão menciona invoice e permite payments. Conhecimento database dos outros projetos é proibido. | 20 | 12 | 8 |
| code_signal | Arquivo de checkout da API exige R16/R17 e permite payments; arquivo NOTIFY_URL de web ou API exige R19 e permite notifications. O vínculo não permite itens privados dos outros projetos. | 20 | 12 | 8 |
| database | O cwd fixa API, web ou billing; exige respectivamente R04, R07 ou R10 e proíbe as outras duas interpretações. Nenhum vínculo é ativado pela palavra genérica. | 20 | 12 | 8 |
| supersession | Pede cursor-policy v3 (D3), refund-policy v3 com checkout (D6, R05, R16), ou receipt-policy v3 com receipt (D9, R19). As duas versões anteriores são proibidas. | 20 | 12 | 8 |
| local_release | Release-check nos quatro componentes exige L1 e a lei local. Checkout release-check da API exige também L6 e o contrato R05/R16 de payments. Notas expiradas são proibidas. | 20 | 12 | 8 |
| Total | | 120 | 72 | 48 |

Cada tipo tem cinco sessões de quatro turnos, com prompts únicos. O campo
`family` identifica o cenário, inclusive quando reaparece em sessões distintas.
Família não é sinônimo de sessão: por exemplo, as três sessões de links
acessíveis compartilham `web-accessible-links`. Toques não migram entre sessões,
o cwd permanece estável e `event_seq` começa em 1. Caminhos são relativos ao
workspace; `.` representa a raiz. `now` é `2026-03-01T12:00:00Z`.

Mantemos `local_release`: estes cartões testam leis ativadas pela keyword
`release-check`, não uma regra always-on. Nenhum item tem `always: true`.
Os casos de release têm objetivos diferentes dos consertos de implementação:
são critérios de aceitação com contas inventadas e preservação das leis locais.

### Famílias e split

O split é atribuído por cenário antes de qualquer ajuste; sessões da mesma
família nunca são distribuídas entre dev e test. Cada tipo mantém 12/8 eventos,
totalizando 72/48. A divisão não segue mais o número ordinal das sessões.

| Tipo | Famílias dev (sessões) | Famílias test (sessões) |
| --- | --- | --- |
| abstention | wallpaper (1), desk-plants (2), meeting-title (3) | literal-text (4), desk-cable (5) |
| explicit_project | web-accessible-links (1, 3, 5) | billing-rounding (2, 4) |
| code_signal | shared-service-url (2, 3, 5) | adapter-money-key (1, 4) |
| database | server-ledger (1, 4), journal-retention (3) | browser-cache (2, 5) |
| supersession | opaque-cursor (1, 4), separate-receipt-routes (3) | refund-cents-key (2, 5) |
| local_release | release-order-ids (1), release-accessible-previews (2), release-integer-cents (3) | release-sandbox (4), release-checkout-key (5) |

As famílias agrupam o problema semântico, não apenas a redação. Por isso, a URL
compartilhada nos clientes web e API permanece toda em dev. Em contrapartida,
a passagem de centavos e chave pelo adaptador fica toda em test. Compartilhar
um item necessário não implica compartilhar o cenário: a regra de centavos pode
ser necessária tanto ao conserto de reembolso quanto à aceitação de release.
Esta separação não garante cobertura de cada regra nos dois lados; evita usar
reescritas do mesmo cartão para tuning e avaliação.

### Diversidade e rótulos

São 120 prompts distintos após minúsculas e normalização de espaços. A anotação
manual de idioma é explícita: o primeiro turno de cada uma das 30 sessões está
em português, os demais em inglês (25%). Há frases de tamanhos variados, erros
de digitação deliberados e 19 eventos com código entre crases; um desses contém
um bloco multilinha delimitado por três crases. Essa contagem não é detecção
automática de idioma.

Trechos de código, tanto inline quanto cercados por três crases, são dados, não
evidência textual para gatilhos. `literal-text` inclui `database`, `checkout`,
`receipt`, `release-check`, `audit`, `clock`, `invoice` e `refund-policy` somente
entre crases, com required vazio e os respectivos itens explicitamente
proibidos. As outras famílias com código têm sua evidência fora das crases ou
em `tool_paths`. O carregador não é um resolvedor de gatilhos: estes rótulos são
o oráculo para o futuro harness, e os testes protegem esses cartões negativos.

Os rótulos foram revistos conforme a tabela e a história: `code_signal` usa os
arquivos tocados para o contrato; `database` nunca abre um vínculo; o texto
literal não ativa escopos privados; versões antigas continuam proibidas.
Required representa necessidade de conteúdo, inclusive em turnos da mesma
família, não uma ordem de repetir o corpo após dedup. O futuro harness deve
aplicar métricas de sessão e cobertura de leis conforme EVALUATION. Este v0 não
implementa seleção, dedup, métricas de recuperação nem compaction.

O teste calcula Jaccard por conjuntos de palavras minúsculas, separando em
qualquer caractere não alfanumérico Unicode, sem stemming ou remoção de
stopwords; inclui também palavras de código para uma comparação conservadora.
Ele emite aviso para cada par dev×test acima de 0,8 e falha se houver algum.
Máximo atual: 0,285714 (`local_release-2-4` × `local_release-4-4`).

Este é um conjunto inicial de rótulos sintéticos, não uma alegação de revisão
por dois anotadores. A sobreposição humana de 20% e o κ de Cohen previstos em
EVALUATION ainda não foram realizados. Mudanças futuras de rótulos devem ser
isoladas e revisadas antes de qualquer tuning; não se deve ajustar usando test.

## Carregamento e validação

`elo_ctx::bench::load_fixture("bench/fixture/v0")` lê os quatro arquivos de
configuração/dados e valida ids, referências, escopos, disjunção dos rótulos,
partição exata do split, isolamento de sessões e famílias, prompts duplicados
normalizados e ausência de ciclos. Ids de itens/eventos, `session_id` e `family`
não podem ser vazios ou conter apenas espaços. `required`, `forbidden` e
`allowed_scopes` não admitem repetições. Todo required deve estar em um escopo
permitido, ativo e, se tiver prazo, com `expires_at > now` (instantes RFC3339).
`superseded_by` existe se e somente se o status for `superseded`; cada sucessor
mantém kind e escopo, e a ponta da cadeia deve estar ativa. Leis têm origem
`user`, no máximo 400 bytes UTF-8 e `always: false`; nenhuma keyword específica
é exigida pelo formato. Prompt presente não pode ser vazio ou só espaços;
sem prompt, `tool_paths` deve conter ao menos um caminho. Os tipos públicos
estão em `elo_ctx::bench::fixture`. Erros estruturais têm variantes próprias;
erros JSON informam arquivo, linha e coluna reais (inclusive em `split.json`). `serde_json::from_str` desserializa structs Serde com
`deny_unknown_fields`; Cargo.lock registra a versão resolvida da dependência
`serde_json = "1.0"`, sem fixação por `=`. Cada erro JSONL informa a linha física,
incluindo linhas vazias inválidas.

`tests/fixture_test.rs` carrega v0 e também constrói fixtures temporários para
cada erro, incluindo limite UTF-8, referências inexistentes, self-cycle,
ciclo entre itens, JSON inválido, campos desconhecidos/duplicados, split
incompleto ou duplicado, vazamento de família/sessão e diversidade de prompts.
