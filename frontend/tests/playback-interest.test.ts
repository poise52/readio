import { test } from 'node:test'
import { strict as assert } from 'node:assert'
import { PlaybackInterest } from '../app/utils/playback-interest.ts'

test('out-of-order updates after 20 jumps and pause cannot revive stale work', async () => {
  const requests: { revision: number, blocks: string[], resolve: (accepted: boolean) => void }[] = []
  const interest = new PlaybackInterest('session', (_session, revision, blocks) => new Promise(resolve => requests.push({ revision, blocks, resolve })))
  const pending = Array.from({ length: 20 }, (_, i) => interest.update([`block-${i}`]))
  const pause = interest.update([])
  assert.deepEqual(requests.at(-1)?.blocks, [])
  for (const request of requests.toReversed()) request.resolve(true)
  assert.deepEqual(await Promise.all(pending), Array(20).fill(null))
  assert.deepEqual(await pause, { session: 'session', revision: 21 })
  const resumed = interest.update(['block-19'])
  requests.at(-1)!.resolve(true)
  assert.deepEqual(await resumed, { session: 'session', revision: 22 })
})

test('backend rejection prevents request dispatch', async () => {
  const interest = new PlaybackInterest('session', async () => false)
  await assert.rejects(interest.update(['block']), /Сессия воспроизведения/)
})
