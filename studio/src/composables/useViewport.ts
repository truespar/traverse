import { ref, onBeforeUnmount } from 'vue'

const MOBILE_QUERY = '(max-width: 768px)'

const isMobile = ref(false)
let mql: MediaQueryList | null = null
let listener: ((e: MediaQueryListEvent) => void) | null = null
let refcount = 0

function ensureListener() {
  if (mql) return
  mql = window.matchMedia(MOBILE_QUERY)
  isMobile.value = mql.matches
  listener = (e) => { isMobile.value = e.matches }
  mql.addEventListener('change', listener)
}

function releaseListener() {
  if (refcount > 0) return
  if (mql && listener) mql.removeEventListener('change', listener)
  mql = null
  listener = null
}

export function useViewport() {
  ensureListener()
  refcount++
  onBeforeUnmount(() => {
    refcount--
    if (refcount <= 0) releaseListener()
  })
  return { isMobile }
}
