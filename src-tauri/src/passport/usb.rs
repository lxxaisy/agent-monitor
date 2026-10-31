use super::{coalesce_packets, encode_line, CompanionService, LineDecoder, IO_TIMEOUT};
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Receiver, Arc};
use std::time::{Duration, Instant};

pub fn run(
    service: Arc<CompanionService>,
    target: &str,
    receiver: &Receiver<Value>,
    stop: Arc<AtomicBool>,
    generation: u64,
) -> Result<(), String> {
    let mut port = serialport::new(target, 115200)
        .timeout(IO_TIMEOUT)
        .open()
        .map_err(|error| format!("无法打开 USB 串口: {error}"))?;
    let mut decoder = LineDecoder::new();
    let mut buffer = [0u8; 1024];
    let began = Instant::now();
    let mut last_received = Instant::now();
    let mut handshake = false;
    // The probe is a protocol message only; it never toggles DTR/RTS or enters a bootloader.
    port.write_all(&encode_line(
        &serde_json::json!({"v":1,"type":"hello","host":"agent-monitor"}),
    )?)
    .map_err(|error| error.to_string())?;
    while !stop.load(Ordering::Acquire) {
        for packet in coalesce_packets(receiver) {
            // The device drains 256 bytes per USB cycle from a 4 KiB receive ring.
            // Unpaced writes larger than that ring lose bytes on USB Serial/JTAG.
            for chunk in encode_line(&packet)?.chunks(256) {
                port.write_all(chunk)
                    .map_err(|error| format!("USB 写入失败: {error}"))?;
                port.flush().map_err(|error| error.to_string())?;
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        match port.read(&mut buffer) {
            Ok(0) => return Err("USB 设备已断开".into()),
            Ok(size) => {
                for message in decoder.push(&buffer[..size]) {
                    last_received = Instant::now();
                    if message["type"] == "hello" && message["device"] == "ai-passport" {
                        handshake = true;
                    }
                    service.receive(generation, message);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(format!("USB 读取失败: {error}")),
        }
        if !handshake && began.elapsed() > Duration::from_secs(12) {
            return Err("串口已打开，但未收到 AI Passport 新固件握手".into());
        }
        if handshake && last_received.elapsed() > Duration::from_secs(20) {
            return Err("设备超过 20 秒没有确认数据，连接已失效".into());
        }
    }
    Ok(())
}
