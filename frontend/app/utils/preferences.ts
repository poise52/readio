import type { ModelSettings, Preferences } from "../types/models";

export function modelSettingsSnapshot(
  settings: Preferences["openRouterModelSettings"] | null | undefined,
): Preferences["openRouterModelSettings"] {
  return Object.fromEntries(
    Object.entries(settings ?? {}).map(([id, value]) => [
      id,
      {
        resourceRevision: value.resourceRevision,
        transport: value.transport
          ? {
              format: value.transport.format,
              pcmSampleRate: value.transport.pcmSampleRate,
              pcmChannels: value.transport.pcmChannels,
            }
          : value.transport,
        inputLimit: value.inputLimit
          ? {
              unit: value.inputLimit.unit,
              value: value.inputLimit.value,
              source: value.inputLimit.source,
              verifiedAt: value.inputLimit.verifiedAt,
            }
          : value.inputLimit,
      },
    ]),
  );
}

export function normalizePreferences(
  defaults: Preferences,
  incoming: Partial<Preferences>,
): Preferences {
  return {
    ...defaults,
    ...incoming,
    openRouterModelSettings: modelSettingsSnapshot(
      incoming.openRouterModelSettings,
    ),
    openRouterBillingUnits: {
      ...(incoming.openRouterBillingUnits ?? defaults.openRouterBillingUnits),
    },
    dictionary: { ...(incoming.dictionary ?? defaults.dictionary) },
  };
}

export function modelSettingsWithRevision(
  settings: Preferences["openRouterModelSettings"] | null | undefined,
  model: string,
  revision: string,
): Preferences["openRouterModelSettings"] {
  return {
    ...settings,
    [model]: { ...settings?.[model], resourceRevision: revision },
  };
}

export function modelSettingsWithLimit(
  settings: ModelSettings | undefined,
  transport: NonNullable<ModelSettings["transport"]>,
  inputLimit: ModelSettings["inputLimit"],
): ModelSettings {
  return { ...settings, transport, inputLimit: inputLimit ?? null };
}

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
    openRouterModelSettings: modelSettingsSnapshot(
      prefs.openRouterModelSettings,
    ),
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
    openRouterModelSettings: modelSettingsSnapshot(
      current.openRouterModelSettings,
    ),
  };
}
