import type { OfflineJobState } from "./ipc";

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
  length: number;
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
export interface AudioPosition {
  chapter: number;
  blockId: string;
  profileId: string;
  seconds: number;
}
export interface Book extends BookSummary {
  audioPosition: AudioPosition | null;
  chapters: Chapter[];
  marks: Mark[];
  passages: Passage[];
}
export interface ModelSettings {
  resourceRevision?: string | null;
  transport?: {
    format: "mp3" | "pcm";
    pcmSampleRate?: number | null;
    pcmChannels?: number | null;
  } | null;
  inputLimit?: {
    unit: "character" | "utf8_byte" | "xml_escaped_byte";
    value: number;
    source: "user" | "provider";
    verifiedAt?: number | null;
  } | null;
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
  openRouterModelSettings: Record<string, ModelSettings>;
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
  canonicalSlug: string | null;
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
export interface SpeechTiming {
  startUtf16: number;
  endUtf16: number;
  startSeconds: number;
  endSeconds: number;
}
export interface SpeechAudio {
  timings?: SpeechTiming[] | null;
  data?: string;
  resourceId?: string;
  mime: string;
  playbackRate?: number;
}
export interface AudioPart {
  synthesisStart?: number | null;
  synthesisEnd?: number | null;
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
  text?: string;
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
  state: OfflineJobState;
  totalBlocks: number;
  cachedBlocks: number;
  pendingRequests: number;
  submittedRequests: number;
  unknownCostRequests: number;
  actualCostUsd: string | null;
  estimatedReservedUsd: string | null;
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
  pricingSource: "openrouter_api" | null;
  pricingVerifiedAt: number | null;
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

export interface OfflineDiagnostic {
  file: string;
  bookId: string | null;
  profileId: string | null;
  message: string;
}
