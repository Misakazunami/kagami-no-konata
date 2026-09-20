/**
 * 把文本写入剪贴板
 *
 * `navigator.clipboard` 在 Tauri WebView 的非安全上下文下会被拒绝，
 * 因此保留旧式 `execCommand` 兜底（原先内联在 ChangesPanel 的复制报告里）。
 */
export async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // WebView 在非安全上下文下会拒绝 clipboard API
  }
  const area = document.createElement("textarea");
  area.value = text;
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.appendChild(area);
  area.select();
  try {
    document.execCommand("copy");
  } finally {
    document.body.removeChild(area);
  }
}
