# BuWH/cc-switch — Fork 维护笔记

本 fork 在上游 `farion1231/cc-switch` 基础上叠加少量 patch,采用**轻量 fork + rebase**策略:
不删任何功能,只加需要的 commit,定期 rebase 到上游最新,自己本地 build 使用,不等上游 merge。

## 本 fork 的 patch(相对上游 main)

分支 `feat/codex-copilot-provider`,三个 commit:

1. **`fix(proxy): treat empty choices as valid empty response on both transform paths`**
   - 修复 Claude Code auto 权限模式下 Bash/edit 被锁死的 bug。
   - GitHub Copilot 的 Claude 模型在 `max_tokens=1` 分类请求下可能返回 200 + 空 `choices: []`,
     上游把它当致命错误 → 422 → Claude Code 锁死所有副作用操作。
   - 改动:`transform.rs`(Claude 路径)+ `transform_codex_chat.rs`(Codex chat 路径)——
     空数组降级为合法空响应;字段缺失仍报错。
   - 对应上游未 merge 的 PR #4923(本 fork 更完整,覆盖了两条路径)。

2. **`feat(codex): support GitHub Copilot as a Codex provider (backend)`**
   - `codex.rs`:`extract_auth` 在 `is_github_copilot()` 时返回 `AuthStrategy::GitHubCopilot`;
     `get_auth_headers` 发 Copilot 指纹头。
   - `codexProviderPresets.ts`:`CodexProviderPreset` 加 `providerType` / `requiresOAuth`,
     新增 GitHub Copilot preset(base_url `api.githubcopilot.com`,`apiFormat: openai_chat`)。
   - **无需改 forwarder / codex_config.rs**:token 注入由 `AuthStrategy` 驱动、adapter 无关;
     PROXY_MANAGED 占位 + base_url 重写 + 官方 auth.json 保留走现有 takeover 路径,天然复用。

3. **`feat(codex): wire Copilot auth section into Codex provider form (frontend)`**
   - Codex 表单接入现有 `CopilotAuthSection` + `useCopilotAuth`,gated on `providerType === github_copilot`。
   - 账号绑定 / submit guard / providerType 持久化本就 app-agnostic,扩展 `isCopilotProvider` 认 Codex 分支后自动生效。

## Rebase 到上游

```bash
git fetch upstream
git checkout feat/codex-copilot-provider
git rebase upstream/main
# 冲突面小(仅 4 个文件),解决后:
cd src-tauri && cargo test --lib codex transform && cd ..
bun run typecheck
```

## 本地并存(与正式版 cc-switch 隔离)

正式版用 `~/.cc-switch` + 端口 15721。dev 版用环境变量隔离,避免互相覆盖 provider 状态:

```bash
export CC_SWITCH_TEST_HOME="$HOME/.cc-switch-dev"   # 重定向 DB + .claude + .codex
cd /path/to/ccsw && bun run tauri dev
# 首次启动后在 dev UI 里把 proxy 端口改成 15722(避免撞 15721)
```

## 正式版替换安全规则

正式版是当前 Codex token provider，禁止采用“先退出、再执行复制/启动命令”的多步交互
流程。旧版退出后 agent 可能断连，导致新版永远没有机会启动。

统一使用 `scripts/replace-cc-switch-app.sh`：它先在线完成预检和 staging，再把停旧版、
原子替换、启动、`/health` 验证及失败回滚交给独立的用户域 launchd job。命令返回
`SCHEDULED` 后，当前 agent 必须立即结束回复，不再执行后续工具调用。

## Codex + Copilot 使用步骤

1. dev 版 UI → Codex → 添加 provider → 选 "GitHub Copilot" preset
2. 完成 Copilot OAuth 登录(复用 Claude 侧同一套 device flow)
3. 切换到该 provider(takeover 会写 PROXY_MANAGED 占位,保留官方 auth.json → Remote control 仍可用)
4. Codex 的 `~/.codex/config.toml` base_url 指向本地代理;代理把 Responses 转 chat/completions 发给 Copilot
