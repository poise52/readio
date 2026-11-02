import { test } from 'node:test'
import { strict as assert } from 'node:assert'
import { replayIndex, playbackRate, synthesizeThenPlay, configurePlaybackRate } from '../app/utils/playback.ts'

test('pause during preview synthesis discards late audio before playback starts', async () => {
  let current = true, played = false, release!: (audio: string) => void
  const pending = new Promise<string>(resolve => { release = resolve })
  const playing = synthesizeThenPlay(() => pending, async () => { played = true }, () => current)
  current = false
  release('audio')
  await playing
  assert.equal(played, false)
})

test('audio speed preserves pitch and adapts to synthesized speed', () => {
  const audio = { playbackRate: 1, preservesPitch: false }
  configurePlaybackRate(audio, 1.7)
  assert.deepEqual(audio, { playbackRate: 1.7, preservesPitch: true })
  configurePlaybackRate(audio)
  assert.equal(audio.playbackRate, 1)
  assert.ok(Math.abs(playbackRate(1, 1.5, 2.1) - 1.4) < 1e-9)
  assert.ok(Math.abs(playbackRate(1.5, 1.5, 2.1) - 2.1) < 1e-9)
  assert.equal(replayIndex(true, 9), 0)
  assert.equal(replayIndex(false, 9), 9)
})
