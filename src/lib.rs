use astrobox_ng_wit::FutureReader;
use astrobox_ng_wit::astrobox::psys_host::{
    device, dialog, interconnect, register, thirdpartyapp, ui,
};
use astrobox_ng_wit::exports::astrobox::psys_plugin::{
    event::{self, EventType},
    lifecycle,
};
use base64::Engine;
use serde_json::{json, Value};
use std::sync::Mutex;

// Doom 接收器快应用包名（与 doom1/src/manifest.json 的 package 一致）
const PKG_NAME: &str = "com.doom.wadloader";
// 与接收器 index.ux 的 CHUNK_SIZE 一致
const CHUNK_SIZE: usize = 1536;
const EVENT_SEND_WAD: &str = "doom_send_wad";

// ===== 全局状态 =====
struct Transfer {
    filename: String,
    data: Vec<u8>,
    next: usize,
    total: usize,
}

struct State {
    device_addr: Option<String>,
    status: String,
    transfer: Option<Transfer>,
    root: Option<String>,
}

static S: Mutex<State> = Mutex::new(State {
    device_addr: None,
    status: String::new(),
    transfer: None,
    root: None,
});

// ===== 工具 =====
fn set_status(s: &str) {
    S.lock().unwrap().status = s.to_string();
}

fn status_text() -> String {
    S.lock().unwrap().status.clone()
}

fn rerender() {
    let root = S.lock().unwrap().root.clone();
    if let Some(root) = root {
        let el = build_ui();
        ui::render(&root, el);
    }
}

// 快应用回执可能被宿主包一层 payloadText / payload
fn extract_payload_text(p: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(p) {
        if let Some(t) = v.get("payloadText").and_then(|x| x.as_str()) {
            return t.to_string();
        }
        if let Some(payload) = v.get("payload") {
            if let Some(t) = payload.as_str() {
                return t.to_string();
            }
            return payload.to_string();
        }
    }
    p.to_string()
}

// ===== 发送基础 =====
fn send_json(addr: &str, obj: Value) -> bool {
    let s = obj.to_string();
    let r = astrobox_ng_wit::block_on(async {
        interconnect::send_qaic_message(addr, PKG_NAME, &s).await
    });
    r.is_ok()
}

fn send_start(addr: &str, filename: &str, size: usize) -> bool {
    send_json(addr, json!({"type": "start_transfer", "filename": filename, "size": size}))
}

fn send_chunk(addr: &str, index: usize, b64: &str) -> bool {
    send_json(addr, json!({"type": "file_chunk", "chunkIndex": index, "data": b64}))
}

fn send_complete(addr: &str) -> bool {
    send_json(addr, json!({"type": "transfer_complete"}))
}

// 每收到一个 file_chunk_received 回执，发下一块
fn advance_transfer() {
    enum Action {
        Chunk(usize, String),
        Done,
    }

    let addr = match S.lock().unwrap().device_addr.clone() {
        Some(a) => a,
        None => return,
    };

    let action = {
        let mut st = S.lock().unwrap();
        let tr = match st.transfer.as_mut() {
            Some(t) => t,
            None => return,
        };
        if tr.next >= tr.total {
            st.transfer = None;
            Action::Done
        } else {
            let start = tr.next * CHUNK_SIZE;
            let end = (start + CHUNK_SIZE).min(tr.data.len());
            let b64 = base64::engine::general_purpose::STANDARD.encode(&tr.data[start..end]);
            let idx = tr.next;
            tr.next += 1;
            st.status = format!("发送 {}/{} 块", tr.next, tr.total);
            Action::Chunk(idx, b64)
        }
    };

    match action {
        Action::Chunk(idx, b64) => {
            send_chunk(&addr, idx, &b64);
        }
        Action::Done => {
            send_complete(&addr);
            set_status("完成：wad 已发送");
        }
    }
    rerender();
}

// ===== 选择文件并开始发送 =====
fn pick_and_send() {
    // 1. 选文件（dialog 直接返回文件内容）
    let config = dialog::PickConfig { read: true, copy_to: None };
    let filter = dialog::FilterConfig {
        multiple: false,
        extensions: vec!["wad".to_string()],
        default_directory: String::new(),
        default_file_name: String::new(),
    };
    let picked = astrobox_ng_wit::block_on(async { dialog::pick_file(&config, &filter).await });
    if picked.data.is_empty() {
        set_status("未选择文件或文件为空");
        rerender();
        return;
    }
    let filename = picked.name.clone();
    let data = picked.data.clone();
    let size = data.len();

    // 2. 找已连接设备
    let connected = astrobox_ng_wit::block_on(async { device::get_connected_device_list().await });
    let addr = match connected.first() {
        Some(dev) => dev.addr.clone(),
        None => {
            set_status("无已连接设备，请先在 AstroBox 连接手表");
            rerender();
            return;
        }
    };

    // 3. 注册接收（收 file_chunk_received 回执）
    let _ = astrobox_ng_wit::block_on(async {
        register::register_interconnect_recv(&addr, PKG_NAME).await
    });

    // 3.1 启动接收器快应用（interconnect 消息需要它在运行）
    let apps = astrobox_ng_wit::block_on(async {
        thirdpartyapp::get_thirdparty_app_list(&addr).await
    })
    .unwrap_or_default();
    if let Some(app) = apps.iter().find(|a| a.package_name == PKG_NAME) {
        let _ = astrobox_ng_wit::block_on(async {
            thirdpartyapp::launch_qa(&addr, app, "pages/index").await
        });
    }

    // 4. 保存传输状态
    let total = (size + CHUNK_SIZE - 1) / CHUNK_SIZE;
    {
        let mut st = S.lock().unwrap();
        st.device_addr = Some(addr.clone());
        st.transfer = Some(Transfer { filename: filename.clone(), data, next: 0, total });
        st.status = format!("开始发送 {}（{} 块）", filename, total);
    }

    // 5. 发 start_transfer，然后发第一块
    send_start(&addr, &filename, size);
    advance_transfer();
}

// ===== UI =====
fn text(content: &str, size: u32, color: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::P, Some(content))
        .size(size)
        .text_color(color)
}

fn button(content: &str, event_id: &str, color: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::Button, Some(content))
        .on(ui::Event::Click, event_id)
        .bg(color)
        .padding(10)
        .radius(10)
        .margin_top(8)
        .width_full()
}

fn build_ui() -> ui::Element {
    let status = status_text();
    ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .padding(12)
        .bg("#07090D")
        .child(text("Doom WAD 发送", 24, "#FFFFFF"))
        .child(text("选择 wad 文件发送到手表", 13, "#9AA6B8"))
        .child(text(&status, 13, "#8FB5FF"))
        .child(button("选择并发送 wad", EVENT_SEND_WAD, "#0D6EFF"))
}

// ===== Guest 实现 =====
struct DoomWadSender;

impl lifecycle::Guest for DoomWadSender {
    fn on_load() {
        astrobox_ng_wit::spawn(async move {
            let connected = device::get_connected_device_list().await;
            if let Some(dev) = connected.first() {
                let addr = dev.addr.clone();
                let _ = register::register_interconnect_recv(&addr, PKG_NAME).await;
                {
                    let mut st = S.lock().unwrap();
                    st.device_addr = Some(addr);
                    st.status = "已连接，点击发送 wad".to_string();
                }
            } else {
                set_status("请先在 AstroBox 连接手表");
            }
            rerender();
        });
    }
}

impl event::Guest for DoomWadSender {
    fn on_event(event_type: EventType, event_payload: String) -> FutureReader<String> {
        let (writer, reader) = astrobox_ng_wit::wit_future::new::<String>(|| "".to_string());
        if matches!(event_type, EventType::InterconnectMessage) {
            let text = extract_payload_text(&event_payload);
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if v.get("type").and_then(|x| x.as_str()) == Some("file_chunk_received") {
                    advance_transfer();
                }
            }
        }
        astrobox_ng_wit::spawn(async move {
            let _ = writer.write("ok".to_string()).await;
        });
        reader
    }

    fn on_ui_event(
        event_id: String,
        event: event::Event,
        _payload: String,
    ) -> FutureReader<String> {
        let (writer, reader) = astrobox_ng_wit::wit_future::new::<String>(|| "".to_string());
        if event_id == EVENT_SEND_WAD
            && matches!(event, event::Event::Click | event::Event::PointerUp)
        {
            pick_and_send();
        }
        astrobox_ng_wit::spawn(async move {
            let _ = writer.write("accepted".to_string()).await;
        });
        reader
    }

    fn on_ui_render(element_id: String) -> FutureReader<()> {
        let (writer, reader) = astrobox_ng_wit::wit_future::new::<()>(|| ());
        {
            S.lock().unwrap().root = Some(element_id.clone());
        }
        let el = build_ui();
        ui::render(&element_id, el);
        astrobox_ng_wit::spawn(async move {
            let _ = writer.write(()).await;
        });
        reader
    }

    fn on_card_render(_card_id: String) -> FutureReader<()> {
        let (writer, reader) = astrobox_ng_wit::wit_future::new::<()>(|| ());
        astrobox_ng_wit::spawn(async move {
            let _ = writer.write(()).await;
        });
        reader
    }
}

astrobox_ng_wit::export!(DoomWadSender);
