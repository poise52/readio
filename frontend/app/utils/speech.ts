const xmlEscapes: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&apos;",
};

export function speechChunks(text: string, limit = 3500): string[] {
  const chunks: string[] = [];
  let chunk = "";
  let bytes = 0;
  const encoder = new TextEncoder();
  for (const char of text) {
    const size = encoder.encode(xmlEscapes[char] ?? char).length;
    if (bytes + size > limit && chunk) {
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
