import { test } from 'node:test'
import { strict as assert } from 'node:assert'
import { filterModels, selectionForModel } from '../app/utils/openrouter.ts'
import type { OpenRouterModel } from '../app/types/models.ts'

const models: OpenRouterModel[] = [
  { id: 'provider/free', name: 'Free Speech', description: 'English', pricing: { prompt: '0', completion: '0' }, voices: ['one', 'two'], free: true },
  { id: 'provider/paid', name: 'Paid Speech', description: 'Русский', pricing: { prompt: '0.00001', completion: '0.1' }, voices: [], free: false },
]

test('catalog search and free/paid filters use the provider classification', () => {
  assert.deepEqual(filterModels(models, '', 'free').map(m => m.id), ['provider/free'])
  assert.deepEqual(filterModels(models, 'РУССКИЙ', 'paid').map(m => m.id), ['provider/paid'])
  assert.deepEqual(filterModels(models, 'free', 'paid'), [])
  assert.deepEqual(filterModels(models, 'provider/', 'all').map(m => m.id), ['provider/free', 'provider/paid'])
})

test('explicit model selection resets voice to that model’s first voice or default', () => {
  assert.deepEqual(selectionForModel(models[0]), { model: 'provider/free', voice: 'one' })
  assert.deepEqual(selectionForModel(models[1]), { model: 'provider/paid', voice: null })
  assert.deepEqual(selectionForModel(undefined), { model: null, voice: null })
})
