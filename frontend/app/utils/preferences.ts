import type { Preferences } from "../types/models";

export function preferencesSnapshot(prefs: Preferences): Preferences {
  return {
    dark: prefs.dark,
    fontSize: prefs.fontSize,
    lineHeight: prefs.lineHeight,
    primaryColor: prefs.primaryColor,
    backgroundImagePath: prefs.backgroundImagePath,
    backgroundOpacity: prefs.backgroundOpacity,
    backgroundDim: prefs.backgroundDim,
    backgroundBlurEnabled: prefs.backgroundBlurEnabled,
    backgroundBlur: prefs.backgroundBlur,
    surfaceTransparency: prefs.surfaceTransparency,
    voice: prefs.voice,
    openRouterModel: prefs.openRouterModel,
    openRouterVoice: prefs.openRouterVoice,
    dictionary: { ...prefs.dictionary },
  };
}
