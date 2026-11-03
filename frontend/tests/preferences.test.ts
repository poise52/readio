import { test } from "node:test";
import { strict as assert } from "node:assert";
import { reactive, isReactive } from "vue";
import {
  preferencesSnapshot,
  withCurrentBillingUnits,
  billingUnitDirty,
  modelSettingsWithLimit,
  modelSettingsWithRevision,
  normalizePreferences,
} from "../app/utils/preferences.ts";
import type { Preferences } from "../app/types/models.ts";

test("preference snapshots contain only allowed settings and copy the dictionary", () => {
  const prefs = {
    dark: true,
    fontSize: 19,
    lineHeight: 1.65,
    primaryColor: 0xff69d9b1,
    backgroundImagePath: null,
    backgroundOpacity: 0.24,
    backgroundDim: 0.2,
    backgroundBlurEnabled: false,
    backgroundBlur: 0.4,
    surfaceTransparency: 0.18,
    voice: null,
    openRouterModel: null,
    openRouterBillingUnits: {},
    openRouterModelSettings: {},
    openRouterVoice: null,
    volume: 0.7,
    dictionary: { hello: "hi" },
    apiKeyDraft: "secret",
  } satisfies Preferences & { apiKeyDraft: string };
  const snapshot = preferencesSnapshot(prefs);
  assert.equal("apiKeyDraft" in snapshot, false);
  prefs.dictionary.hello = "changed";
  assert.equal(snapshot.dictionary.hello, "hi");
});

test("queued preferences preserve billing units saved after the snapshot", () => {
  const prefs = {
    dark: true,
    fontSize: 19,
    lineHeight: 1.65,
    primaryColor: 0xff69d9b1,
    backgroundImagePath: null,
    backgroundOpacity: 0.24,
    backgroundDim: 0.2,
    backgroundBlurEnabled: false,
    backgroundBlur: 0.4,
    surfaceTransparency: 0.18,
    voice: null,
    openRouterModel: null,
    openRouterBillingUnits: {},
    openRouterModelSettings: {},
    openRouterVoice: null,
    volume: 1,
    dictionary: {},
  } satisfies Preferences;
  const queued = preferencesSnapshot(prefs);
  const latest: Preferences = {
    ...prefs,
    openRouterBillingUnits: { "example/model": "utf8_byte" },
  };
  const merged = withCurrentBillingUnits(queued, latest);
  assert.deepEqual(merged.openRouterBillingUnits, {
    "example/model": "utf8_byte",
  });
  assert.equal(
    billingUnitDirty(
      "example/model",
      "character",
      merged.openRouterBillingUnits,
    ),
    true,
  );
  assert.equal(
    billingUnitDirty(
      "example/model",
      "utf8_byte",
      merged.openRouterBillingUnits,
    ),
    false,
  );
});

test("reactive Vue settings become independent plain snapshots and merge into queued saves", () => {
  const prefs = reactive({
    dark: true,
    fontSize: 19,
    lineHeight: 1.65,
    primaryColor: 0,
    backgroundImagePath: null,
    backgroundOpacity: 0.24,
    backgroundDim: 0.2,
    backgroundBlurEnabled: false,
    backgroundBlur: 0.4,
    surfaceTransparency: 0.18,
    voice: "openrouter",
    openRouterModel: "model",
    openRouterVoice: null,
    openRouterBillingUnits: {},
    openRouterModelSettings: {
      model: {
        resourceRevision: "vendor/model-v1",
        transport: {
          format: "pcm" as const,
          pcmSampleRate: 24000,
          pcmChannels: 1,
        },
        inputLimit: {
          unit: "character" as const,
          value: 1000,
          source: "user" as const,
          verifiedAt: 123,
        },
      },
    },
    volume: 1,
    dictionary: {},
  } satisfies Preferences);
  assert.equal(isReactive(prefs.openRouterModelSettings.model.transport), true);
  const snapshot = preferencesSnapshot(prefs);
  const merged = withCurrentBillingUnits(snapshot, prefs);
  assert.equal(
    isReactive(snapshot.openRouterModelSettings.model?.transport),
    false,
  );
  prefs.openRouterModelSettings.model.transport.pcmSampleRate = 48000;
  prefs.openRouterModelSettings.model.inputLimit.value = 2000;
  assert.equal(
    snapshot.openRouterModelSettings.model?.transport?.pcmSampleRate,
    24000,
  );
  assert.equal(merged.openRouterModelSettings.model?.inputLimit?.value, 1000);
  assert.equal(
    merged.openRouterModelSettings.model?.resourceRevision,
    "vendor/model-v1",
  );
});

test("clearing a model input limit removes the previously persisted value", () => {
  const existing = {
    resourceRevision: "v1",
    inputLimit: {
      unit: "character" as const,
      value: 1000,
      source: "user" as const,
    },
  };
  const changed = modelSettingsWithLimit(existing, { format: "mp3" }, null);
  assert.equal(changed.inputLimit, null);
  assert.equal(changed.resourceRevision, "v1");
  assert.equal(existing.inputLimit.value, 1000);
});

test("first model selection handles a reactive bootstrap response without model settings", () => {
  const wire = reactive(
    JSON.parse(
      `{"dark":true,"fontSize":19,"lineHeight":1.65,"primaryColor":0,"backgroundImagePath":null,"backgroundOpacity":0.24,"backgroundDim":0.2,"backgroundBlurEnabled":false,"backgroundBlur":0.4,"surfaceTransparency":0.18,"voice":null,"openRouterModel":null,"openRouterBillingUnits":{},"openRouterVoice":null,"volume":1,"dictionary":{}}`,
    ) as Omit<Preferences, "openRouterModelSettings">,
  );
  const defaults: Preferences = { ...wire, openRouterModelSettings: {} };
  assert.deepEqual(
    preferencesSnapshot(wire as Preferences).openRouterModelSettings,
    {},
  );
  const prefs = reactive(normalizePreferences(defaults, wire));
  prefs.openRouterModel = "vendor/model";
  prefs.openRouterModelSettings = modelSettingsWithRevision(
    prefs.openRouterModelSettings,
    "vendor/model",
    "vendor/model-v1",
  );
  const saved = withCurrentBillingUnits(preferencesSnapshot(prefs), prefs);
  assert.equal(saved.openRouterModel, "vendor/model");
  assert.equal(
    saved.openRouterModelSettings["vendor/model"]?.resourceRevision,
    "vendor/model-v1",
  );
  assert.deepEqual(modelSettingsWithRevision(undefined, "vendor/model", "v1"), {
    "vendor/model": { resourceRevision: "v1" },
  });
});
