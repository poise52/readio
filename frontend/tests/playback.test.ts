import { test } from 'node:test'
import { strict as assert } from 'node:assert'
import { replayIndex, runPlayback } from '../app/utils/playback.ts'

test('cancelling pending synthesis prevents late position advancement', async () => {
  let current = true, advanced = 0, release!: () => void
  const pending = new Promise<void>(resolve => { release = resolve })
  const playing = runPlayback({ current: () => 'First.', text: item => item, before: () => {}, speak: () => pending, advance: async () => { advanced++; return true }, isCurrent: () => current })
  current = false
  release()
  await playing
  assert.equal(advanced, 0)
})

test('plays across chapter boundary, skips punctuation and finishes once', async () => {
  const chapters = [['One.', '***'], ['Two.']]
  let chapter = 0, offset = 0, finished = 0
  const spoken: string[] = []
  await runPlayback({ current: () => chapters[chapter]?.[offset] ?? null, text: item => item, before: () => {}, speak: async text => { spoken.push(text) }, advance: async () => { if (++offset < chapters[chapter]!.length) return true; if (++chapter < chapters.length) { offset = 0; return true } finished++; return false }, isCurrent: () => true })
  assert.deepEqual(spoken, ['One.', 'Two.'])
  assert.equal(finished, 1)
  assert.equal(replayIndex(true, 9), 0)
  assert.equal(replayIndex(false, 9), 9)
})

test('pause during synthesis discards late audio before playback starts', async () => {
  const { synthesizeThenPlay } = await import('../app/utils/playback.ts')
  let current = true, played = false, release!: (audio: string) => void
  const pending = new Promise<string>(resolve => { release = resolve })
  const playing = synthesizeThenPlay(() => pending, async () => { played = true }, () => current)
  current = false
  release('audio')
  await playing
  assert.equal(played, false)
})

test('Remote audio speed is applied locally with pitch preservation', async () => {
  const { configurePlaybackRate } = await import('../app/utils/playback.ts')
  const audio = { playbackRate: 1, preservesPitch: false }
  configurePlaybackRate(audio, 1.7)
  assert.deepEqual(audio, { playbackRate: 1.7, preservesPitch: true })
  configurePlaybackRate(audio)
  assert.equal(audio.playbackRate, 1)
})
