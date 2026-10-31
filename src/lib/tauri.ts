import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import {
  emitTo,
  listen as tauriListen,
  type UnlistenFn,
} from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { getCurrentWindow, type ResizeDirection } from "@tauri-apps/api/window";

export const isNativeApp = () =>
  typeof window !== "undefined" &&
  Boolean(
    (window as unknown as { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__,
  );

export const isLocalPreview = () =>
  !isNativeApp() &&
  new URLSearchParams(window.location.search).get("preview") === "local";

export type AppWindowLabel = "main" | "quick" | "notices";

export function currentWindowLabel(): AppWindowLabel {
  const label = isNativeApp()
    ? getCurrentWindow().label
    : new URLSearchParams(window.location.search).get("window");
  return label === "quick" || label === "notices" ? label : "main";
}

export async function invokeCommand<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  if (isNativeApp()) return tauriInvoke<T>(command, args);
  if (!isLocalPreview())
    throw new Error("请打开 Agent Monitor 桌面应用以读取本机数据。");
  const response = await fetch("/api/command", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ command, args: args ?? {} }),
  });
  if (!response.ok) throw new Error(await response.text());
  return response.json() as Promise<T>;
}

export async function listenEvent<T>(
  event: string,
  handler: (payload: T) => void,
): Promise<UnlistenFn> {
  if (!isNativeApp()) return () => {};
  return tauriListen<T>(event, ({ payload }) => handler(payload));
}

export async function showAppWindow(label: "main" | "quick"): Promise<void> {
  if (!isNativeApp()) {
    const url = new URL(window.location.href);
    url.searchParams.set("window", label);
    window.location.assign(url);
    return;
  }
  await tauriInvoke(label === "quick" ? "show_quick" : "show_main");
}

export async function hideAppWindow(label: AppWindowLabel): Promise<void> {
  if (!isNativeApp()) return;
  const target = await WebviewWindow.getByLabel(label);
  if (target) {
    await target.hide();
    await emitTo(label, "monitor://window-visibility-changed", {
      label,
      visible: false,
    });
  }
}

export async function startCurrentWindowDragging(): Promise<void> {
  if (isNativeApp()) await getCurrentWindow().startDragging();
}

export async function startCurrentWindowResize(
  direction: ResizeDirection,
): Promise<void> {
  if (isNativeApp()) await getCurrentWindow().startResizeDragging(direction);
}
