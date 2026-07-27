import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export async function tauriInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return invoke<T>(cmd, args);
}

export async function tauriListen<T>(event: string, handler: (event: { payload: T }) => void): Promise<UnlistenFn> {
  return listen<T>(event, handler);
}
