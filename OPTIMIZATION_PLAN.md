# Folkbot Optimization Plan

> 基於對整個專案程式碼庫的全面審查，整理出以下優化建議。
> 按優先級分為：高（安全/可靠性）、中（程式碼品質/效能/測試）、低（功能完整性）。

---

## 一、安全（Security）

### 1.1 輪換已洩漏的 API 金鑰和 Bot Token

- **現狀**：`.env` 檔案中包含真實的 Poe API 金鑰和 Telegram bot token。`.folkbot_history` 中也包含敏感資訊。
- **建議**：立即在對應平台上輪換所有金鑰，確保 `.env` 已在 `.gitignore` 中（已確認），並考慮加入 pre-commit hook（如 `detect-secrets` 或 `gitleaks`）防止未來意外提交。
- **影響**：極高 — 任何人取得這些金鑰即可冒充 bot 或消耗 API 額度。

### 1.2 加入 CI/CD 管線

- **現狀**：無 CI 設定（無 `.github/workflows/` 目錄），`CONTRIBUTING.md` 提及 CI 但未實作。
- **建議**：加入 GitHub Actions workflow：
  - `cargo test`（所有測試）
  - `cargo clippy -- -D warnings`（lint）
  - `cargo fmt --check`（格式化檢查）
  - `cargo audit`（依賴安全審計）
- **影響**：中 — 防止回歸錯誤和安全漏洞進入 main 分支。

---

## 二、可靠性（Reliability）

### 2.1 MCP 客戶端加入重試機制

- **現狀**：`src/mcp/mod.rs` 中 MCP 客戶端只有 30 秒固定超時，無重試邏輯。網路波動或 MCP server 短暫不穩定會導致工具呼叫直接失敗。
- **建議**：加入指數退避重試（最多 3 次），超時改為可配置（從 `folkbot.toml` 讀取）。
- **影響**：中 — 提升 MCP 工具在生產環境的穩定性。

### 2.2 優雅關閉機制

- **現狀**：`src/cli/serve.rs` 中後台任務（背景壓縮器、設定監視器）在關閉時直接呼叫 `.abort()`，不等清理完成。
- **建議**：使用 `tokio::CancellationToken` 在全系統廣播關閉訊號，各子任務收到訊號後完成當前工作再退出。實現步驟：
  1. 在 `Bootstrap` 結構中新增 `shutdown_token: CancellationToken`
  2. 將 token 傳遞給所有 spawn 的任務
  3. `serve.rs` 中收到 Ctrl+C 後先 `token.cancel()`，再 `join_all` 等待所有任務結束
  4. 加入超時機制（10 秒後強制終止未完成的任務）
- **影響**：中 — 防止 SQLite WAL 檔案損壞和不完整的訊息寫入。

### 2.3 健康檢查端點

- **現狀**：`serve` 模式沒有任何健康檢查方式，無法被 process manager（systemd、supervisor）或容器編排系統監控。
- **建議**：加入一個簡單的 HTTP 健康檢查端點（如 `GET /health`），回傳 JSON `{"status": "ok"}` 和 200。可選加入更詳細的狀態（DB 連通性、LLM provider 可達性）。
- **影響**：低 — 方便生產部署的監控。

---

## 三、程式碼架構與品質（Code Architecture & Quality）

### 3.1 拆分過大的檔案

- **現狀**：
  - `src/agent.rs`（877 行）：包含 turn 迴圈、系統提示詞組合、狀態管理、媒體處理
  - `src/storage/soul.rs`（757 行）：包含 CRUD、patch 驗證、修訂歷史、鎖定邏輯
- **建議**：
  - `agent.rs` 拆分為：
    - `src/agent/mod.rs` — turn 迴圈和公開介面
    - `src/agent/prompt.rs` — `build_system_prompt()` 和優先級排程（~200 行）
    - `src/agent/state.rs` — `TurnState` 結構和 reload 邏輯（~150 行）
  - `soul.rs` 拆分為：
    - `src/storage/soul/mod.rs` — 公開 API
    - `src/storage/soul/patch.rs` — `apply_patch()` 和驗證邏輯（~300 行）
    - `src/storage/soul/revisions.rs` — 修訂歷史（~150 行）
- **影響**：低 — 改善可讀性和維護性，不改變功能。

### 3.2 SQL 查詢優化

- **現狀**：`src/storage/conversations.rs` 中的 `list_members()` 在應用層遍歷並重新排序結果，而非在 SQL 中排序。
- **建議**：直接在 SQL 查詢中加入 `ORDER BY cm.joined_at`。
- **影響**：低 — 少量效能提升和程式碼簡化。

### 3.3 清理未使用程式碼

- **現狀**：多處 `#[allow(dead_code)]` 標註的欄位：
  - `MediaPart::TextDoc { marker, .. }` 和 `MediaPart::Note { marker, .. }` 的 `marker` 欄位從未被讀取
  - `Fact` 結構中的 `tags` 欄位
  - `Summary` 結構中的 `created_at` 欄位
- **建議**：移除這些未使用欄位，或實作對應功能（如事實標籤過濾）。
- **影響**：低 — 減少混淆和多餘的記憶體使用。

### 3.4 DB_PATH 可配置化

- **現狀**：`src/main.rs` 中 `DB_PATH` 寫死為 `data/folkbot.db`。
- **建議**：改為從 `FOLKBOT_DB_PATH` 環境變數或 CLI `--db-path` 參數讀取，預設保持 `data/folkbot.db`。方便執行多個實例或自訂資料目錄。
- **影響**：低 — 提升部署彈性。

### 3.5 音訊模型名稱可配置化

- **現狀**：`src/llm/audio.rs` 中 Whisper model 名稱寫死為 `"whisper-1"`。
- **建議**：改為從 `folkbot.toml` 的 `[llm]` 段落讀取（如 `audio_model = "whisper-1"`），使用自託管 Whisper 服務時可切換。
- **影響**：低 — 提升部署彈性。

---

## 四、效能（Performance）

### 4.1 背景壓縮器增量處理

- **現狀**：`src/background.rs` 中的壓縮器每次 tick 遍歷所有使用者，即使沒有新訊息也執行檢查。
- **建議**：在 `users` 表中加入 `last_compressed_at` 時間戳，只處理在該時間後有新訊息的使用者。
- **影響**：中 — 減少不必要的 DB 查詢和 LLM API 呼叫，節省成本。

### 4.2 Token 計數精度改善

- **現狀**：`src/tokens.rs` 中的 tiktoken fallback 使用簡單的 CJK/ASCII 判斷（`c >= 0x4E00`），對混合文字（表情符號、阿拉伯文等）估算不準確。
- **建議**：使用 `unicode-width` crate 替代手動 Unicode 範圍判斷，或直接使用 tiktoken 的 `o200k_base` 編碼以匹配較新的模型。
- **影響**：低 — 更精確的 token 計數意味更好的 context window 利用率。

### 4.3 TurnState 跨 Turn 快取

- **現狀**：`TurnState` 在每個 turn 開始時重建，但 soul card 和 tool schemas 很少變化（僅在設定檔變更或資料庫修改時）。
- **建議**：為 soul card 和 tool schemas 加入跨 turn 快取層，使用 `Arc<RwLock<>>` 或 `tokio::watch`，只在設定檔變更（watcher 通知）或 soul patch 工具被呼叫後失效快取。
- **影響**：低 — 減少每個 turn 的 DB 查詢次數。

---

## 五、測試（Testing）

### 5.1 補足儲存層測試

- **現狀**：`storage/facts.rs`、`storage/users.rs`、`storage/conversations.rs` 完全沒有測試。
- **建議**：至少為每個模組加入：
  - `facts.rs`：新增/刪除/列表/去重邏輯
  - `users.rs`：角色管理、密碼雜湊驗證、principal 連結/取消連結
  - `conversations.rs`：建立房間、加入成員、成員列表、標籤搜尋
- **影響**：中 — 儲存層是整個系統的基礎，這些邏輯錯誤會導致資料損壞。

### 5.2 LLM Provider 和 MCP 客戶端測試

- **現狀**：`src/llm/` 和 `src/mcp/` 完全沒有測試。
- **建議**：
  - LLM：使用 mock HTTP server（如 `wiremock`）模擬 OpenAI-compatible API 回應，測試串流解析、錯誤處理、超時
  - MCP：編寫一個簡單的測試用 MCP server（bash script 或 Rust binary），測試初始化握手、工具列表、工具呼叫的完整流程
- **影響**：中 — 這些是外部依賴，bug 會直接影響使用者體驗。

### 5.3 Telegram Adapter 單元測試

- **現狀**：`src/channels/telegram/` 完全沒有測試。
- **建議**：至少為以下邏輯加入單元測試：
  - `inbound.rs`：訊息分類（文字/圖片/語音/貼圖/文件）
  - `addressing.rs`：`is_addressed_to_bot()` 的各種情境（@mention、回覆、暱稱匹配）
  - `streaming.rs`：`chunk_for_telegram()` 的分割邏輯
- **影響**：低 — 減少 Telegram 頻道的回歸錯誤。

---

## 六、功能完整性（Feature Completeness）

### 6.1 實作 Anthropic Provider

- **現狀**：`src/config.rs` 中 `"anthropic"` provider 返回錯誤，引導使用者走 OpenAI-compatible 路徑。
- **建議**：使用 Anthropic Messages API（`/v1/messages`）實作完整的 `LlmProvider`。要點：
  - Anthropic 的 system prompt 是頂層欄位（非 messages 陣列中的元素）
  - 工具定義格式略有不同
  - 串流事件格式不同（`content_block_start` / `content_block_delta`）
- **影響**：中 — 讓使用者能直接使用 Claude 模型。

### 6.2 Discord Channel Adapter

- **現狀**：僅有 Telegram 頻道 adapter。
- **建議**：使用 `serenity` crate 實作 Discord adapter。注意事項：
  - Discord 的訊息長度限制不同（2000 字元）
  - 需要處理 guild/channel 層級的權限
  - 附件和 embed 的上傳方式與 Telegram 不同
- **影響**：中 — 擴展使用者群。

### 6.3 Docker 支援

- **現狀**：無容器化支援，部署需手動安裝 Rust 工具鏈並編譯。
- **建議**：加入 `Dockerfile`（multi-stage build，產生最小化映像檔）和 `docker-compose.yml`（包含 volume 掛載和環境變數設定）。
- **影響**：中 — 大幅降低部署門檻。

### 6.4 監控指標

- **現狀**：只有 tracing 日誌，無結構化指標。
- **建議**：加入以下監控能力（按優先級排序）：
  1. Turn 計數（成功/失敗）和 LLM 延遲（p50/p95/p99）→ 使用 `tracing` 的 span 記錄
  2. 工具呼叫次數和類型分佈
  3. 背景壓縮器執行時間
  4. 可選：Prometheus metrics endpoint（使用 `metrics` + `metrics-exporter-prometheus` crate）
- **影響**：低 — 幫助了解使用模式和效能瓶頸。

---

## 總結優先級矩陣

| 優先級 | 類別 | 項目數 |
|--------|------|--------|
| 高 | 安全 | 2 |
| 高 | 可靠性 | 3 |
| 中 | 程式碼品質 | 5 |
| 中 | 效能 | 3 |
| 中 | 測試 | 3 |
| 低 | 功能完整性 | 4 |

**建議執行順序**：先處理高優先級的安全項目（金鑰輪換 + CI），再依序處理可靠性、測試、程式碼品質和效能，最後再考慮新功能。
