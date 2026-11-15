export type InputSignal = "quiet" | "sound" | "silent" | "disconnected" | "noData";

/** 提示描述已观测到的输入状态，不把无声直接当成硬件静音。 */
export function inputSignalNotice(phase: string, signal?: InputSignal): string | null {
  if (phase !== "recording") return null;
  if (signal === "silent") return "录音中 · 未检测到声音";
  if (signal === "disconnected") return "麦克风已断开";
  if (signal === "noData") return "未收到麦克风音频";
  return null;
}
