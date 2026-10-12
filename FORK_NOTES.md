# BuWH/cc-switch — Fork 维护笔记

本 fork 在上游 `farion1231/cc-switch` 基础上保留兼容修复。`main` 是唯一长期集成
和安装基线；功能分支只用于隔离开发，不能作为已完成修复的永久存放位置。

## 修复交付与安装门禁

1. 每次从最新 `main` 创建独立分支/工作树。修复完成后必须提交、跑回归测试，
   再安全合入本地 `main` 并普通推送 `origin/main`；未合入前不能宣称修复已交付。
2. 发布前逐个检查 `git worktree list --porcelain` 中实际存在工作树的
   `git status --short`、已暂存和未暂存 diff、以及 `main..branch` 提交。
   仅看 branch HEAD 会漏掉未提交的修复。已完成且应保留的修复必须先迁入集成候选；
   未完成或无关 WIP 明确列为暂不发布，保留原现场，不能盲目合并或清理。
3. 必须覆盖真实请求处理路径的回归测试。Copilot 重复消息测试需要同时覆盖普通透传、
   迟到工具参数修复、原生压缩路径，不能只测试 ID 稳定化函数本身。
4. 从干净、已提交的源码构建，记录完整源码 SHA。允许先在隔离候选提交上测试和构建，
   但安装前该精确提交必须成为本地和服务器 `main`，任何期间移动都要停下核对。
5. 最终 staging 应用的 `Info.plist` 必须在签名前包含 `CCSwitchSourceCommit`，
   值为本次实际构建的完整源码 SHA。封装后重新验证签名、DMG、版本、架构与 SHA-256。
   不得给旧包补写新提交号来通过检查；同版本重新修复也必须重建并使用不同产物目录。
6. 安装只能从主工作树调用最新 `scripts/replace-cc-switch-app.sh`，不再调用旧
   feature 工作树脚本。脚本在预检及后台停止 provider 前再次核对包内源码提交、
   干净本地 `main`、`origin/main` 和服务器 `refs/heads/main`。缺少来源或旧提交一律拒绝。
7. 安装仍须用户明确授权；后台任务输出 `SCHEDULED` 后立即结束当前交互。

签名前记录来源（`source_commit` 必须在干净源码构建前记录，并在构建后复核未变化）：

```bash
test -z "$(git status --porcelain)"
source_commit=$(git rev-parse HEAD)
pnpm tauri build --bundles dmg
test "$(git rev-parse HEAD)" = "$source_commit"
test -z "$(git status --porcelain)"
# 将本次新构建应用复制到 staging 后，再写入来源并重新签名、封装和校验。
/usr/libexec/PlistBuddy -c "Add :CCSwitchSourceCommit string $source_commit" \
  "$staged_app/Contents/Info.plist"
codesign --force --deep --sign - "$staged_app"
```

门禁独立回归测试：`zsh scripts/test-release-source.sh`。
自动升级只构建和推送，不得安装；须将上述检查作为发布/替换前置条件。

## 早期 Patch 历史

以下为历史实现记录，部分能力已被上游原生支持；不作为当前开发或发布基线。

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

## 同步上游

```bash
git fetch origin main
git fetch upstream main --tags
# 在从最新 main 创建的独立候选分支中合并 upstream/main。
# 逐项迁入应保留的修复，通过测试/构建/产物验证后才 fast-forward main 并推送。
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

统一使用 main 中的 `scripts/replace-cc-switch-app.sh`：它先在线完成来源预检和 staging，再把停旧版、
原子替换、启动、`/health` 验证及失败回滚交给独立的用户域 launchd job。命令返回
`SCHEDULED` 后，当前 agent 必须立即结束回复，不再执行后续工具调用。

## Codex + Copilot 使用步骤

1. dev 版 UI → Codex → 添加 provider → 选 "GitHub Copilot" preset
2. 完成 Copilot OAuth 登录(复用 Claude 侧同一套 device flow)
3. 切换到该 provider(takeover 会写 PROXY_MANAGED 占位,保留官方 auth.json → Remote control 仍可用)
4. Codex 的 `~/.codex/config.toml` base_url 指向本地代理;代理把 Responses 转 chat/completions 发给 Copilot
