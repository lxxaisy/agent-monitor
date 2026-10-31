use super::{coalesce_packets, encode_line, CompanionService, DeviceTarget, LineDecoder};
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager};
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Receiver, Arc};
use std::time::{Duration, Instant};
use uuid::Uuid;

const SERVICE: Uuid = Uuid::from_u128(0x6e400001b5a3f393e0a9e50e24dcca9e);
const RX: Uuid = Uuid::from_u128(0x6e400002b5a3f393e0a9e50e24dcca9e);
const TX: Uuid = Uuid::from_u128(0x6e400003b5a3f393e0a9e50e24dcca9e);

async fn adapter() -> Result<Adapter, String> {
    // CoreBluetooth keeps a backend thread alive for each adapter. Reuse the
    // adapter across scans and reconnects, on Tauri's persistent async runtime.
    static ADAPTER: tokio::sync::OnceCell<Adapter> = tokio::sync::OnceCell::const_new();
    ADAPTER
        .get_or_try_init(|| async {
            Manager::new()
                .await
                .map_err(|error| error.to_string())?
                .adapters()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .next()
                .ok_or_else(|| "没有可用的蓝牙适配器".into())
        })
        .await
        .cloned()
}

pub fn scan() -> Result<Vec<DeviceTarget>, String> {
    tauri::async_runtime::block_on(async {
        tokio::time::timeout(Duration::from_secs(12), async {
            let adapter = adapter().await?;
            adapter
                .start_scan(ScanFilter {
                    services: vec![SERVICE],
                })
                .await
                .map_err(|error| error.to_string())?;
            tokio::time::sleep(Duration::from_secs(4)).await;
            let mut targets = Vec::new();
            for peripheral in adapter
                .peripherals()
                .await
                .map_err(|error| error.to_string())?
            {
                if let Some(properties) = peripheral
                    .properties()
                    .await
                    .map_err(|error| error.to_string())?
                {
                    if properties.services.contains(&SERVICE) {
                        targets.push(DeviceTarget {
                            id: peripheral.id().to_string(),
                            name: properties
                                .local_name
                                .unwrap_or_else(|| "AI Passport".into()),
                        });
                    }
                }
            }
            let _ = adapter.stop_scan().await;
            Ok(targets)
        })
        .await
        .map_err(|_| "蓝牙扫描超时，请检查系统蓝牙权限与蓝牙开关".to_string())?
    })
}

pub fn run(
    service: Arc<CompanionService>,
    target: &str,
    receiver: &Receiver<Value>,
    stop: Arc<AtomicBool>,
    generation: u64,
) -> Result<(), String> {
    tauri::async_runtime::block_on(async {
        let adapter = adapter().await?;
        adapter
            .start_scan(ScanFilter {
                services: vec![SERVICE],
            })
            .await
            .map_err(|error| error.to_string())?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let peripherals = adapter
            .peripherals()
            .await
            .map_err(|error| error.to_string())?;
        let peripheral = peripherals
            .into_iter()
            .find(|peripheral| peripheral.id().to_string() == target)
            .ok_or("找不到选中的 AI Passport，请重新扫描")?;
        let _ = adapter.stop_scan().await;
        peripheral
            .connect()
            .await
            .map_err(|error| format!("BLE 连接失败: {error}"))?;
        let result: Result<(), String> = async {
            peripheral
                .discover_services()
                .await
                .map_err(|error| error.to_string())?;
            let characteristics = peripheral.characteristics();
            let rx = characteristics
                .iter()
                .find(|item| item.uuid == RX)
                .ok_or("设备未提供 UART RX")?;
            let tx = characteristics
                .iter()
                .find(|item| item.uuid == TX)
                .ok_or("设备未提供 UART TX")?;
            let mut stream = peripheral
                .notifications()
                .await
                .map_err(|error| error.to_string())?;
            // The peripheral can send its encrypted hello as soon as subscription
            // completes. Install the broadcast receiver before enabling it.
            peripheral
                .subscribe(tx)
                .await
                .map_err(|error| error.to_string())?;
            let hello =
                encode_line(&serde_json::json!({"v":1,"type":"hello","host":"agent-monitor"}))?;
            // 20-byte writes fit the mandatory ATT MTU; newline framing is independent of chunk size.
            for chunk in hello.chunks(20) {
                peripheral
                    .write(rx, chunk, WriteType::WithResponse)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            let mut decoder = LineDecoder::new();
            let mut last_received = Instant::now();
            while !stop.load(Ordering::Acquire) {
                // Drain queued hello/receipts before starting another large frame.
                // Otherwise each pending notification waits behind a full snapshot.
                while let Some(value) = stream.next().now_or_never() {
                    let notification = value.ok_or("蓝牙通知流已关闭")?;
                    for message in decoder.push(&notification.value) {
                        last_received = Instant::now();
                        service.receive(generation, message);
                    }
                }
                for packet in coalesce_packets(receiver) {
                    for chunk in encode_line(&packet)?.chunks(20) {
                        if stop.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        // RX explicitly supports encrypted writes without response.
                        // Pace mandatory-MTU chunks instead of paying one round trip
                        // per 20 bytes; the complete frame still requires its receipt.
                        peripheral
                            .write(rx, chunk, WriteType::WithoutResponse)
                            .await
                            .map_err(|error| error.to_string())?;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                tokio::select! {
                    value = stream.next() => {
                        let notification = value.ok_or("蓝牙通知流已关闭")?;
                        for message in decoder.push(&notification.value) {
                            last_received = Instant::now();
                            service.receive(generation,message);
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
                if last_received.elapsed() > Duration::from_secs(25) {
                    return Err("蓝牙设备长时间未确认数据".into());
                }
            }
            Ok(())
        }
        .await;
        let _ = peripheral.disconnect().await;
        result
    })
}

#[cfg(all(test, target_os = "macos"))]
mod resource_tests {
    use super::*;
    use crate::model::Settings;
    use crate::storage::SnapshotStore;

    #[test]
    #[ignore = "requires macOS Bluetooth access; run alone with --test-threads=1"]
    fn repeated_adapter_acquisition_keeps_storage_handles_available() {
        let directory =
            std::env::temp_dir().join(format!("agent-monitor-ble-handles-{}", std::process::id()));
        let store = SnapshotStore::open(directory.join("monitor.db")).unwrap();
        let acquire = || {
            tauri::async_runtime::block_on(async {
                tokio::time::timeout(Duration::from_secs(5), adapter())
                    .await
                    .expect("Bluetooth adapter initialization timed out")
                    .expect("Bluetooth access is required for this resource regression")
            })
        };
        drop(acquire());
        let descriptors = || std::fs::read_dir("/dev/fd").unwrap().count();
        let baseline = descriptors();
        let mut samples = vec![baseline];
        for _ in 0..32 {
            drop(acquire());
            store.save_settings(&Settings::default()).unwrap();
            samples.push(descriptors());
        }
        eprintln!("BLE file descriptors across 32 acquisitions: {samples:?}");
        let after = *samples.last().unwrap();
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            after <= baseline + 3,
            "repeated BLE adapter acquisition leaked file descriptors: {baseline} -> {after}"
        );
    }
}
