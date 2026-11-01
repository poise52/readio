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
  dictionary: Record<string, string>;
}
export interface Voice {
  id: string;
  name: string;
  locale: string;
  engine: string;
}
export interface Bootstrap {
  books: BookSummary[];
  preferences: Preferences;
  voices: Voice[];
  warnings: string[];
}
export interface SpeechAudio {
  data: string;
  mime: string;
}
