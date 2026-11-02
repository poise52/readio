export interface BookSummary {
  id: string;
  title: string;
  author: string;
  cover: string | null;
  chapter: number;
  offset: number;
  speed: number;
  progress: number;
}
export interface Chapter {
  title: string;
  text: string;
}
export interface Mark {
  id: string;
  chapter: number;
  offset: number;
  excerpt: string;
}
export interface Passage {
  start: number;
  end: number;
  text: string;
}
export interface Book extends BookSummary {
  chapters: Chapter[];
  marks: Mark[];
  passages: Passage[];
}
export interface Preferences {
  dark: boolean;
  fontSize: number;
  lineHeight: number;
  primaryColor: number;
  backgroundImagePath: string | null;
  backgroundOpacity: number;
  backgroundDim: number;
  backgroundBlurEnabled: boolean;
  backgroundBlur: number;
  surfaceTransparency: number;
  voice: string | null;
  openRouterModel: string | null;
  openRouterBillingUnits: Record<string, "character" | "utf8_byte">;
  openRouterVoice: string | null;
  volume: number;
  dictionary: Record<string, string>;
}
export interface Voice {
  id: string;
  name: string;
  locale: string;
  engine: string;
}
export interface OpenRouterModel {
  id: string;
  name: string;
  description: string;
  pricing: { prompt: string; completion: string };
  voices: string[];
  free: boolean;
}
export interface OpenRouterStatus {
  hasApiKey: boolean;
}
export interface Bootstrap {
  books: BookSummary[];
  preferences: Preferences;
  voices: Voice[];
  warnings: string[];
  openRouter: OpenRouterStatus;
}
export interface SpeechAudio {
  data: string;
  mime: string;
  playbackRate?: number;
}
export interface AudioPart {
  index: number;
  start: number;
  end: number;
  weight: number;
}
export interface AudioBlock {
  id: string;
  chapter: number;
  start: number;
  end: number;
  passages: AudioPart[];
  text: string;
  characters: number;
}
export interface AudioPlan {
  profileId: string;
  blocks: AudioBlock[];
  offline: boolean;
}
export interface OfflineStatus {
  bookId: string;
  profileId: string | null;
  state: "none" | "paused" | "running" | "complete" | "error";
  totalBlocks: number;
  cachedBlocks: number;
  bytes: number;
  error: string | null;
}
export interface OfflineQuote {
  quoteId: string;
  profileId: string;
  model: string | null;
  engine: string;
  voice: string;
  modelVoice: string | null;
  totalBlocks: number;
  cachedBlocks: number;
  retryRequiredBlocks: number;
  characters: number;
  maxBlockBytes: number;
  maxPassages: number;
  limitSource: "app";
  pricing: { prompt: string; completion: string } | null;
  estimatedCost: string | null;
  costReason:
    | "providerunitunavailable"
    | "additionalrateunitunavailable"
    | "invalidrate"
    | null;
  estimateSource: "user" | "free" | null;
  billableUnits: number | null;
  costUnit: "free" | "unknown" | "character" | "utf8_byte";
  expiresAt: number;
}
