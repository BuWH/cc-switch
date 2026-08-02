# CC Switch Fork — Codex + GitHub Copilot 支持

> 本 fork：`BuWH/cc-switch`（上游 `farion1231/cc-switch`）
> 工作目录：`~/code/cc-switch`，功能分支 `feat/codex-copilot-provider`
> 完成日期：2026-07-10

## 这个 fork 解决了什么

你用 cc-switch 把 GitHub Copilot 作为 provider。原版有两个问题：

1. **Claude Code 在 auto 权限模式下会锁死** —— Bash/编辑操作被拦，报 "claude-opus-4-8 is temporarily unavailable, so auto mode cannot determine the safety of Bash"。你为此发过 PR #4923，上游一直没 merge。
2. **Codex 完全不能用 Copilot 作为 provider** —— 原版没有这条路径。而且即便硬配，Copilot 的模型（尤其 gpt-5.5 / gpt-5.6 系列）也用不了。

这个 fork 两个都解决了，而且 **Codex 现在能用 gpt-5.6-sol/terra/luna（1M context、xhigh reasoning），和你在 Claude Code / VSCode Copilot 里看到的一致**。

## 采用的策略：轻量 fork + rebase

不删任何上游功能（删了反而要处理编译依赖，更累），只在 fork 上叠加需要的 commit，定期 rebase 上游，本地 build 自用，不受上游 merge 与否的制约。

```bash
git fetch upstream
git checkout feat/codex-copilot-provider
git rebase upstream/main
# 冲突面小（只改少数几个文件），解决后重新 build
```

---

## 6 个 commit 做了什么

| Commit | 内容 |
|---|---|
| `e570bc35` | 前端：Codex provider 表单接入现有 `CopilotAuthSection`（复用 Claude 侧的 OAuth 组件） |
| `b6191350` | 后端：Codex adapter 发 `AuthStrategy::GitHubCopilot` + Copilot 指纹头；preset 加 GitHub Copilot 条目 |
| `5485557f` | **修 Claude Code auto 模式锁死**：空 `choices:[]` 降级为合法空响应（`transform.rs` + `transform_codex_chat.rs` 双路径，比原 PR #4923 更完整） |
| `200ce39c` | 实测抓到的 3 个真 bug：URL 多 `/v1` → 404、copilot 识别缺 authBinding 兜底、preset 假模型名 |
| `b83bb424` | FORK_NOTES.md（仓库内维护笔记，本文档的精简版） |
| `37107fb9` | **按模型 vendor 动态选端点** + gpt-5.6 系列支持（见下） |

---

## 核心技术：为什么 Codex 用 Copilot 这么麻烦

### 1. 认证 —— 复用现有机制，不重写
- Codex provider 标记为 copilot 后，`is_github_copilot()` 识别它 → adapter 返回 `AuthStrategy::GitHubCopilot`。
- 真实 Copilot token 由 forwarder 从 `CopilotAuthManager` 动态注入（token 刷新、OAuth 全部复用 Claude 侧已有的）。
- 写 config 时用 `PROXY_MANAGED` 占位符，**官方 `auth.json` 登录不被污染** → Remote control、官方 auth 保留等高级功能天然可用（走现有 takeover 路径）。

### 2. 格式转换 —— Responses ↔ Chat
- Codex 客户端发 OpenAI Responses API 请求。
- Copilot 的部分模型只认 `/chat/completions`，代理需双向转换（`transform_codex_chat`，上游已有）。

### 3. 最关键的坑：Copilot 每个模型只支持一个端点
实测发现（真实 Copilot enterprise）：

| 模型 | 支持的端点 |
|---|---|
| gpt-5.5、gpt-5.6-sol/terra/luna（OpenAI vendor） | **只走 `/responses`** |
| gpt-4o、gpt-4.1、claude-* | **只走 `/chat/completions`** |

**这就是为什么 Claude Code 能用 gpt-5.5、Codex 不行**：Claude 路径本来就有 `is_copilot_openai_vendor_model` 按模型动态选端点；Codex 路径原本一刀切转 chat，所以 gpt-5.x 系列全部失败。

`37107fb9` 给 Codex 路径补上了这套机制：
- `forwarder.rs`：按请求模型的 vendor 动态决定是否转 chat；决策结果通过 `ForwardResult.codex_converted_to_chat` 传播出去。
- `handlers.rs`：响应回转（chat→responses）改用传播的决策，与请求端**对称**，避免把 responses 透传响应误当 chat 解析（之前报 "No choices in chat response"）。

---

## gpt-5.6 系列实测结论

三个变体 **sol / terra / luna**（gpt-5.5 只有单个），特性完全一致：

- **端点**：`/responses` 透传
- **thinking effort**：`none / low / medium / high / xhigh / max` —— **不支持 `minimal`**
  - "extra high" = `xhigh`
  - config 里填 `model_reasoning_effort = "xhigh"`（或 `max`）
- **context window**：1M 级（实测 400k–750k tokens 通过）
  - context 由 cc-switch preset catalog 决定（写进 `~/.codex/cc-switch-model-catalog.json`），**不是从 Copilot 拉取**
  - 想改大小就改 preset 的 `contextWindow`

preset 默认：`model = gpt-5.6-sol`、`model_reasoning_effort = xhigh`，catalog 含三变体（1M）+ gpt-5.5 + gpt-4o + claude 系列。

---

## 本地 build 与使用

### 前置
```bash
cd ~/code/cc-switch
git checkout feat/codex-copilot-provider
```
工具链：cargo（Rust）、pnpm、node。

### Build 正式版（替换现有安装版）
```bash
pnpm install
pnpm tauri build
# 产物：src-tauri/target/release/bundle/macos/CC Switch.app
```
identifier/version 和现有正式版一致，新版会**无缝接管** `~/.cc-switch` 现有数据和 Copilot 登录。

adhoc 签名（无开发者证书）+ 除隔离：
```bash
xattr -cr "src-tauri/target/release/bundle/macos/CC Switch.app"
codesign --force --deep --sign - "src-tauri/target/release/bundle/macos/CC Switch.app"
```

### 替换步骤（provider-safe）

CC Switch 本身是 Codex/Claude Code 的 provider。不能在一个交互流程里先退出旧版，
再依赖后续命令复制和启动新版：旧版一停，当前 agent 连接也可能立即中断。

必须使用仓库内的 launchd 托管替换脚本。脚本会在 provider 仍在线时完成 DMG 校验、
staging 和 ad-hoc 签名，然后把退出、原子替换、启动、健康检查与失败回滚交给独立的
用户域 launchd job。默认延迟 30 秒，给当前 agent 足够时间返回“已调度”结果。

```bash
# 只做完整预检，不替换
scripts/replace-cc-switch-app.sh \
  --dmg "src-tauri/target/release/bundle/dmg/CC Switch_3.18.0_aarch64.dmg" \
  --expected-version 3.18.0 \
  --check-only

# 调度替换。命令返回 SCHEDULED 后不要再执行工具调用，立即结束当前 agent 回复。
scripts/replace-cc-switch-app.sh \
  --dmg "src-tauri/target/release/bundle/dmg/CC Switch_3.18.0_aarch64.dmg" \
  --expected-version 3.18.0
```

launchd worker 会自动:

1. 优雅退出旧版，必要时发送 `TERM`；
2. 在 `/Applications` 内原子换名，保留旧版作为临时回滚备份；
3. 启动新版并等待 `http://127.0.0.1:15721/health`；
4. 新版失败时恢复并启动旧版；
5. 写入 `/private/tmp/cc-switch-replace-latest.status` 和 `.log`，并发系统通知。

连接恢复后的下一次会话先检查:

```bash
cat /private/tmp/cc-switch-replace-latest.status
tail -n 100 /private/tmp/cc-switch-replace-latest.log
```

### Codex + Copilot 使用
1. UI → Codex → 添加 provider → 选 "GitHub Copilot" preset
2. 完成 Copilot OAuth 登录（复用 Claude 侧同一套 device flow；若正式版已登录则自动带过来）
3. 切换到该 provider（takeover 写 PROXY_MANAGED 占位，保留官方 auth.json）
4. Codex 的 `~/.codex/config.toml` base_url 指向本地代理，代理按模型自动选端点

---

## 开发/测试隔离（并存两个实例）

调试新改动时，可让 dev 版和正式版同时跑、互不干扰：

```bash
# dev 版用独立配置目录 + 独立端口
CC_SWITCH_TEST_HOME=$HOME/.cc-switch-dev pnpm tauri dev
# 首次启动后在 dev UI 把 proxy 端口改成 15722（正式版是 15721）
```
- `CC_SWITCH_TEST_HOME` 重定向整个 home（DB + .claude + .codex），完全隔离。
- 若要两个 GUI 同时开，还需临时改 `src-tauri/tauri.conf.json` 的 `identifier`（如 `com.ccswitch.desktop.dev`）避开单实例锁 —— **此改动不要提交**。
- 复用正式版 Copilot 登录免 OAuth：`cp ~/.cc-switch/copilot_auth.json ~/.cc-switch-dev/.cc-switch/`

### 直接测代理（不经 Codex CLI）
```bash
# Codex Responses 格式，打 dev 端口
curl -sS -X POST http://127.0.0.1:15722/v1/responses \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-5.6-sol","input":"say ok","reasoning":{"effort":"xhigh"},"stream":false}'
```

---

## 改动涉及的文件（rebase 时留意冲突）

| 文件 | 改动 |
|---|---|
| `src-tauri/src/proxy/providers/transform.rs` | 空 choices 降级 |
| `src-tauri/src/proxy/providers/transform_codex_chat.rs` | 空 choices 降级 |
| `src-tauri/src/proxy/providers/codex.rs` | GitHubCopilot auth 策略 + 指纹头 + build_url 去 /v1 |
| `src-tauri/src/provider.rs` | `is_github_copilot()` 认 authBinding 兜底 |
| `src-tauri/src/proxy/forwarder.rs` | 按 vendor 动态选端点 + 决策传播 |
| `src-tauri/src/proxy/handlers.rs` | 响应回转用传播的决策 |
| `src/config/codexProviderPresets.ts` | GitHub Copilot preset + gpt-5.6 变体 |
| `src/components/providers/forms/*` | Codex 表单接入 CopilotAuthSection |

新增单测：`build_url_copilot_no_v1`、`is_github_copilot_via_auth_binding_without_provider_type`、空 choices 双路径测试。

---

## 待办 / 注意事项

- **未 push 到 remote**：6 个 commit 目前只在本地。要备份就 `git push origin feat/codex-copilot-provider`。
- **一个上游测试会失败**：`update_current_claude_desktop_provider_syncs_profile_when_proxy_takeover_is_active` 硬编码端口 15721，只要有 cc-switch 实例在跑就会因端口冲突失败 —— 这是上游测试的缺陷，非本 fork 回归。
- **无 macOS 开发者证书**：build 只能 adhoc 签名，首次打开需右键→打开过 Gatekeeper。
