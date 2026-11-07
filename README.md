# trem-native-rust

TREM 各個原生桌面 app（Rust + winit + wgpu，不用 WebView）共用的底層：TREM-Lite 的 native 版與 TREM Monitor 的 native 版都建立在這上面。

這裡的程式碼最早是 TREM-Lite native 的一部分，從 [ExpTechTW/TREM-Lite](https://github.com/ExpTechTW/TREM-Lite) 的 `apps/native` 抽出來（`native/winit-wgpu` 分支，`43b2d23`）。這個 repo 的歷史從抽取開始，更早的歷史（為什麼這樣寫、踩過哪些坑）在 TREM-Lite 的 commit 與程式碼註解裡。

## Crate

| crate | 內容 | features |
|---|---|---|
| `trem-core` | 純計算，瀏覽器也能跑：地區與警戒框、走時表、震度換算與色票、版本標籤、強度模型的執行器與各鄉鎮預估、SSE 的格式。地區資料跟著它 | `ml`（預設）：強度模型執行器<br>`rayon`：大批模型輸入分給執行緒池<br>`encode`：從 JSON 產生內嵌的 `.bin` |
| `trem-io` | 不碰 GPU 的 I/O：async 工作在哪裡跑（`exec`）、計時器、桌面的 HTTP 快取代理（http、cache、endpoints）與瀏覽器的 fetch、fixture、檔案日誌、資料目錄 | `cache`（預設）：桌面的 SQLite 快取代理<br>`testing`：計時器的虛擬時鐘 |
| `trem-gfx` | 畫面與它周圍的系統：GPU device、文字、UI 與通用元件、向量地圖（相機、圖磚、map pass 的 pipeline 與 draw 基本操作、sprite）、視窗、平台層、金檔測試工具。重新匯出 `wgpu` 與 `winit` | `dem`：地形陰影<br>`tray`、`autostart`、`notify`、`speech`：系統匣、開機啟動、通知、語音<br>`awake`：隱藏時不降速<br>`testing`：金檔工具與地圖的虛擬時鐘 |

相依方向：`trem-core` ← `trem-io` ← `trem-gfx`。不需要畫面的程式（例如 app 的後端）只依賴前兩個。

## 規則

- **lib 不認得任何 app**：名稱、識別碼、單一實例的 port、URL、字型、顏色、圖層順序都由 app 傳入（`Paths::new`、`ProxyConfig`、`LogConfig`、`FontConfig`、`MapConfig`、`InstanceConfig`、`TrayConfig`……）。地圖只提供基本操作，圖層順序由各 app 的 `Overlay` 實作自己組。
- **給 app 測試用的東西不能用 `#[cfg(test)]`**：app 跑測試時，lib 的 `cfg(test)` 不成立。要用 `testing` feature，由 app 在 `[dev-dependencies]` 打開。
- **`env!` 在 lib 裡讀到的是 lib 自己的值**：版本、`CARGO_MANIFEST_DIR` 都是 lib 的。要用 app 的值，就由 app 傳進來（例如 User-Agent）。
- **log target**：模組路徑維持 `render::`、`map::`、`ui::`、`net::` 等。app 的 `LogConfig.own_crates` 要列出 `trem_core`、`trem_io`、`trem_gfx`，否則它們的 debug log 會消失，target 也不會縮短。
- **錯誤處理**：不可用 `unwrap`/`expect` 處理執行期資料（網路、檔案、JSON）。一個模組失敗不能拖垮其他模組。
- **註解與 log**：註解用英文，log 訊息用繁體中文。
- **檢查**：`cargo fmt` 要乾淨；`cargo clippy` 在桌面與 `--target wasm32-unknown-unknown` 都不能有警告；`cargo test` 要過。

## 在 app 裡使用

三個 crate 釘**同一個 rev**。`[dev-dependencies]` 打開 `testing` 時也要用同一個 git 來源與 rev，否則 Cargo 會把它們當成兩個不相容的 crate。

```toml
[dependencies]
trem-core = { git = "https://github.com/ExpTechTW/trem-native-rust", rev = "<commit>" }
trem-io   = { git = "https://github.com/ExpTechTW/trem-native-rust", rev = "<commit>" }
trem-gfx  = { git = "https://github.com/ExpTechTW/trem-native-rust", rev = "<commit>", features = ["tray"] }

[dev-dependencies]
trem-gfx  = { git = "https://github.com/ExpTechTW/trem-native-rust", rev = "<commit>", features = ["testing"] }
```

要同時改 lib 與 app 時，在 app 寫一份**不進版控**的 `.cargo/config.toml`，把依賴 patch 到本機的這個 repo：

```toml
[patch."https://github.com/ExpTechTW/trem-native-rust"]
trem-core = { path = "../trem-native-rust/trem-core" }
trem-io   = { path = "../trem-native-rust/trem-io" }
trem-gfx  = { path = "../trem-native-rust/trem-gfx" }
```

lib 的改動 push 之後，app 把 `rev` 升上來，再用 `cargo update -p trem-gfx` 更新 lock。不要提交 patch 狀態下的 `Cargo.lock`。

改 lib 時，兩個 app 的測試也要跑：它們的金檔（畫面、事件軌跡）才是 lib 行為的最終規格。

## 開發

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy --workspace --target wasm32-unknown-unknown -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --all-features
```

Linux 需要 D-Bus、speech-dispatcher 與 fontconfig 的開發套件，以及 clang。

## 授權

程式碼以 [AGPL-3.0](LICENSE) 授權。`trem-gfx/testdata/fonts` 的 Manrope 與 Noto Sans TC 只供測試使用，以 [SIL Open Font License 1.1](trem-gfx/testdata/fonts/OFL.txt) 授權。
