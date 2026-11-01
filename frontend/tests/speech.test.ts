import { test } from 'node:test'
import { strict as assert } from 'node:assert'
import { pronunciation, speechChunks } from '../app/utils/speech.ts'

test('chunks preserve Unicode text and remain under synthesis byte limit', () => {
  const source = '😀Привет, мир! '.repeat(2000)
  const chunks = speechChunks(source)
  assert.equal(chunks.join(''), source)
  assert.ok(chunks.length > 1)
  assert.ok(chunks.every(chunk => new TextEncoder().encode(chunk).length <= 7000))
})

test('dictionary matches whole words and replaces literal dollar signs', () => {
  assert.equal(pronunciation('Кот, котик и КОТ.', { кот: '$1' }), '$1, котик и $1.')
})
