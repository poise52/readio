import { test } from 'node:test'
import { strict as assert } from 'node:assert'
import { shouldPollOffline, shouldRefreshPausedQuote, isStaleQuoteError } from '../app/utils/offline-refresh.ts'
import type { OfflineStatus, OfflineQuote } from '../app/types/models.ts'

const paused = { bookId: 'book', state: 'paused', cachedBlocks: 1, totalBlocks: 2 } as OfflineStatus
const quote = { cachedBlocks: 0 } as OfflineQuote

test('polling survives dialog closure after pause and stops at its deadline', () => {
  const deadlines = new Map([['book', 65_000]])
  assert.equal(shouldPollOffline([paused], deadlines, 64_999), true)
  assert.equal(shouldPollOffline([paused], deadlines, 65_000), false)
  assert.equal(shouldPollOffline([{ ...paused, state: 'running' }], new Map(), 70_000), true)
})

test('late cached block triggers a new quote only for an open, idle paused dialog', () => {
  assert.equal(shouldRefreshPausedQuote(paused, quote, true, false), true)
  assert.equal(shouldRefreshPausedQuote(paused, quote, false, false), false)
  assert.equal(shouldRefreshPausedQuote(paused, quote, true, true), false)
  assert.equal(shouldRefreshPausedQuote(paused, { ...quote, cachedBlocks: 1 }, true, false), false)
  assert.equal(isStaleQuoteError('Текст книги или кеш изменились; запросите оценку снова'), true)
  assert.equal(isStaleQuoteError('Сеть недоступна'), false)
})
