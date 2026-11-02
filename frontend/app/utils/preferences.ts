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
    openRouterBillingUnits: { ...prefs.openRouterBillingUnits },
    openRouterVoice: prefs.openRouterVoice,
    volume: prefs.volume,
    dictionary: { ...prefs.dictionary },
  };
}

export function billingUnitDirty(
  model: string | null,
  draft: "" | "character" | "utf8_byte",
  units: Preferences["openRouterBillingUnits"],
): boolean {
  return !!model && draft !== (units[model] ?? "");
}

export function withCurrentBillingUnits(
  snapshot: Preferences,
  current: Preferences,
): Preferences {
  return {
    ...snapshot,
    openRouterBillingUnits: { ...current.openRouterBillingUnits },
  };
}
