import { test } from 'node:test'
import { strict as assert } from 'node:assert'
import { preferencesSnapshot, withCurrentBillingUnits, billingUnitDirty } from '../app/utils/preferences.ts'
import type { Preferences } from '../app/types/models.ts'

test('preference snapshots contain only allowed settings and copy the dictionary', () => {
  const prefs = { dark: true, fontSize: 19, lineHeight: 1.65, primaryColor: 0xff69d9b1, backgroundImagePath: null, backgroundOpacity: .24, backgroundDim: .2, backgroundBlurEnabled: false, backgroundBlur: .4, surfaceTransparency: .18, voice: null, openRouterModel: null, openRouterBillingUnits: {}, openRouterVoice: null, volume: .7, dictionary: { hello: 'hi' }, apiKeyDraft: 'secret' } satisfies Preferences & { apiKeyDraft: string }
  const snapshot = preferencesSnapshot(prefs)
  assert.equal('apiKeyDraft' in snapshot, false)
  prefs.dictionary.hello = 'changed'
  assert.equal(snapshot.dictionary.hello, 'hi')
})


test('queued preferences preserve billing units saved after the snapshot', () => {
  const prefs = { dark: true, fontSize: 19, lineHeight: 1.65, primaryColor: 0xff69d9b1, backgroundImagePath: null, backgroundOpacity: .24, backgroundDim: .2, backgroundBlurEnabled: false, backgroundBlur: .4, surfaceTransparency: .18, voice: null, openRouterModel: null, openRouterBillingUnits: {}, openRouterVoice: null, volume: 1, dictionary: {} } satisfies Preferences
  const queued = preferencesSnapshot(prefs)
  const latest: Preferences = { ...prefs, openRouterBillingUnits: { 'example/model': 'utf8_byte' } }
  const merged = withCurrentBillingUnits(queued, latest)
  assert.deepEqual(merged.openRouterBillingUnits, { 'example/model': 'utf8_byte' })
  assert.equal(billingUnitDirty('example/model', 'character', merged.openRouterBillingUnits), true)
  assert.equal(billingUnitDirty('example/model', 'utf8_byte', merged.openRouterBillingUnits), false)
})
