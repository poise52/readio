import { test } from 'node:test'
import { strict as assert } from 'node:assert'
import { speechChunks } from '../app/utils/speech.ts'

test('preview chunks preserve Unicode text and remain within escaped XML byte limit', () => {
  const source = '😀Привет, <мир> & друзья! '.repeat(2000)
  const chunks = speechChunks(source)
  const replacements: Record<string, string> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&apos;' }
  assert.equal(chunks.join(''), source)
  assert.ok(chunks.length > 1)
  assert.ok(chunks.every(chunk => new TextEncoder().encode(chunk.replace(/[&<>"']/g, char => replacements[char]!)).length <= 3500))
})
