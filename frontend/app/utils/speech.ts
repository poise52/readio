export function pronunciation(
  text: string,
  dictionary: Record<string, string>,
) {
  for (const [word, replacement] of Object.entries(dictionary)) {
    const key = word.trim();
    if (!key) continue;
    const escaped = key.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    text = text.replace(
      new RegExp(`(?<![A-Za-zА-Яа-яЁё])${escaped}(?![A-Za-zА-Яа-яЁё])`, "giu"),
      () => replacement,
    );
  }
  return text;
}
export function speechChunks(text: string): string[] {
  const chunks: string[] = [];
  let chunk = "";
  let bytes = 0;
  const encoder = new TextEncoder();
  for (const char of text) {
    const size = encoder.encode(char).length;
    if (bytes + size > 7000 && chunk) {
      chunks.push(chunk);
      chunk = "";
      bytes = 0;
    }
    chunk += char;
    bytes += size;
  }
  if (chunk) chunks.push(chunk);
  return chunks;
}
