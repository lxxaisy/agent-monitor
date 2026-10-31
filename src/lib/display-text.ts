function isAttachmentMetadata(line: string): boolean {
  return (
    /^## [^\n]+: (?:\/|[A-Za-z]:\\)/.test(line) ||
    /^Image attachment: (?:true|false)$/.test(line)
  );
}

function removeLeadingAttachments(value: string): string {
  const lines = value.split("\n");
  if (lines[0] !== "# Files mentioned by the user:") return value;
  let index = 1;
  while (index < lines.length && lines[index].trim() === "") index += 1;
  if (index === lines.length || !isAttachmentMetadata(lines[index])) return value;
  while (index < lines.length && (lines[index].trim() === "" || isAttachmentMetadata(lines[index]))) index += 1;
  return lines.slice(index).join("\n");
}

export function executionTitleDetails(title: string): string {
  let text = title.replace(/\r\n?/g, "\n").trim();
  text = text.replace(/^<in-app-browser-context\b[^>]*>[\s\S]*?<\/in-app-browser-context>\s*(?:## My request:\s*)?/, "");
  return removeLeadingAttachments(text).trim();
}

export function executionTitle(title: string): string {
  return executionTitleDetails(title).split("\n")[0]
    .replace(/^\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/, "$1")
    .replace(/\s+/g, " ").trim();
}
