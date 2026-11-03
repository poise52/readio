import type { AudioBlock, AudioPart, SpeechTiming } from "../types/models.ts";

export interface TimedPart {
  part: AudioPart;
  start: number;
  end: number;
}
export function timedParts(
  block: AudioBlock,
  timings: SpeechTiming[] | null | undefined,
  duration: number,
): TimedPart[] | null {
  if (
    !timings?.length ||
    !Number.isFinite(duration) ||
    duration <= 0 ||
    !block.passages.length
  )
    return null;
  let previousText = 0,
    previousTime = 0;
  for (const mark of timings) {
    if (
      !Number.isSafeInteger(mark.startUtf16) ||
      !Number.isSafeInteger(mark.endUtf16) ||
      mark.startUtf16 < previousText ||
      mark.endUtf16 <= mark.startUtf16 ||
      !Number.isFinite(mark.startSeconds) ||
      !Number.isFinite(mark.endSeconds) ||
      mark.startSeconds < previousTime ||
      mark.endSeconds <= mark.startSeconds ||
      mark.endSeconds > duration + 0.25
    )
      return null;
    previousText = mark.endUtf16;
    previousTime = mark.endSeconds;
  }
  const parts: TimedPart[] = [];
  let previousPartEnd = 0;
  for (const part of block.passages) {
    if (
      !Number.isSafeInteger(part.synthesisStart) ||
      !Number.isSafeInteger(part.synthesisEnd) ||
      part.synthesisStart! < previousPartEnd ||
      part.synthesisEnd! <= part.synthesisStart!
    )
      return null;
    const matches = timings.filter(
      (mark) =>
        mark.startUtf16 >= part.synthesisStart! &&
        mark.endUtf16 <= part.synthesisEnd!,
    );
    if (!matches.length) return null;
    previousPartEnd = part.synthesisEnd!;
    parts.push({
      part,
      start: matches[0]!.startSeconds,
      end: matches[matches.length - 1]!.endSeconds,
    });
  }
  if (
    timings.some(
      (mark) =>
        !block.passages.some(
          (part) =>
            mark.startUtf16 >= part.synthesisStart! &&
            mark.endUtf16 <= part.synthesisEnd!,
        ),
    )
  )
    return null;
  return parts;
}
export function timedSeek(parts: TimedPart[], offset: number): number | null {
  return (
    parts.find(({ part }) => part.start <= offset && offset < part.end)
      ?.start ?? null
  );
}
export function timedPosition(
  parts: TimedPart[],
  seconds: number,
): number | null {
  let selected = parts[0];
  for (const part of parts) {
    if (part.start > seconds) break;
    selected = part;
  }
  return selected?.part.start ?? null;
}
