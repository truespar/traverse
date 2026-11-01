<script setup lang="ts">
import { computed } from 'vue'
import { useSettingsStore } from '../../stores/settings'
import type { PanelId } from '../../stores/settings'
import { useConnectionStore } from '../../stores/connection'
import { api } from '../../utils/api'

const settings = useSettingsStore()
const conn = useConnectionStore()

// Server reports compile-time feature flags via /api/health.
// We hide the ML Pipelines button when the running binary was
// built without `--features ml` - same gating as the GQL
// dropdown. Defaults to visible until the first health response
// lands so the icon doesn't flicker on initial paint.
const showMlPanel = computed(() => {
  const f = conn.features
  return f && Object.keys(f).length > 0 ? f.ml === true : true
})

// Capabilities are baked in at boot when `setApiClient` swaps the
// backend, so reading them once at script setup time is sufficient.
// The HTTP backend reports every feature available; the WASM backend
// has no users, so that button is hidden entirely.
// The assistant also needs a model on the server (health `features.ai`).
const showUsers = api.capabilities.users
const showAiChat = computed(() => conn.aiChatAvailable)

function toggle(panel: PanelId) {
  settings.activePanel = settings.activePanel === panel ? null : panel
}
</script>

<template>
  <nav class="activity-bar">
    <!-- Logo (click to toggle dashboard) -->
    <button
      class="activity-bar__logo"
      :class="{ 'activity-bar__logo--active': settings.activePanel === 'dashboard' }"
      title="Dashboard"
      @click="toggle('dashboard')"
    >
      <div class="activity-bar__logo-icon">
        <svg viewBox="0 0 512 512" fill="none" xmlns="http://www.w3.org/2000/svg">
          <path d="M227.3 371.862V136.3H277.435L278.989 182.548L269.662 179.05C271.216 171.018 275.621 163.505 282.876 156.509C290.13 149.255 298.94 143.425 309.303 139.021C319.926 134.616 331.067 132.414 342.726 132.414C360.863 132.414 376.927 137.078 390.918 146.405C404.909 155.473 415.92 168.039 423.952 184.103C431.984 199.907 436 218.173 436 238.9C436 259.109 431.984 277.246 423.952 293.309C415.92 309.373 404.909 322.068 390.918 331.396C376.927 340.464 360.992 344.998 343.115 344.998C331.197 344.998 319.797 342.666 308.915 338.002C298.033 333.339 288.835 327.38 281.321 320.125C273.807 312.611 269.014 304.58 266.941 296.03L279.378 290.589V427L227.3 371.862ZM331.844 299.916C342.467 299.916 351.795 297.325 359.827 292.143C368.118 286.961 374.595 279.836 379.259 270.768C383.922 261.441 386.254 250.818 386.254 238.9C386.254 226.723 383.922 216.1 379.259 207.032C374.595 197.705 368.247 190.45 360.215 185.268C352.183 180.087 342.726 177.496 331.844 177.496C321.222 177.496 311.765 180.087 303.474 185.268C295.442 190.45 289.094 197.705 284.43 207.032C279.767 216.1 277.435 226.723 277.435 238.9C277.435 250.818 279.767 261.441 284.43 270.768C289.094 279.836 295.442 286.961 303.474 292.143C311.765 297.325 321.222 299.916 331.844 299.916Z" fill="currentColor"/>
          <path d="M115.253 342.277V213.638V85H166.942V213.638V342.277H115.253ZM76 184.102V137.077H210.47V184.102H76Z" fill="currentColor"/>
        </svg>
      </div>
    </button>

    <!-- Nav icons -->
    <div class="activity-bar__icons">
      <!-- Schema -->
      <button
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.activePanel === 'schema' }"
        title="Schema"
        @click="toggle('schema')"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <ellipse cx="12" cy="5" rx="9" ry="3" />
          <path d="M3 5V19A9 3 0 0 0 21 19V5" />
          <path d="M3 12A9 3 0 0 0 21 12" />
        </svg>
      </button>

      <!-- Files -->
      <button
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.activePanel === 'files' }"
        title="Files"
        @click="toggle('files')"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2" />
        </svg>
      </button>

      <!-- Users - hidden entirely in WASM mode (no auth concept). -->
      <button
        v-if="showUsers"
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.activePanel === 'users' }"
        title="Users"
        @click="toggle('users')"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" />
          <path d="M16 3.128a4 4 0 0 1 0 7.744" />
          <path d="M22 21v-2a4 4 0 0 0-3-3.87" />
          <circle cx="9" cy="7" r="4" />
        </svg>
      </button>

      <!-- Import -->
      <button
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.activePanel === 'import' }"
        title="Import"
        @click="toggle('import')"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 15V3" />
          <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
          <path d="m7 10 5 5 5-5" />
        </svg>
      </button>

      <!-- Examples -->
      <button
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.activePanel === 'examples' }"
        title="Examples"
        @click="toggle('examples')"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12.89 1.45l8 4A2 2 0 0 1 22 7.24v9.53a2 2 0 0 1-1.11 1.79l-8 4a2 2 0 0 1-1.79 0l-8-4a2 2 0 0 1-1.1-1.8V7.24a2 2 0 0 1 1.11-1.79l8-4a2 2 0 0 1 1.78 0z" />
          <polyline points="2.32 6.16 12 11 21.68 6.16" />
          <line x1="12" x2="12" y1="22.76" y2="11" />
        </svg>
      </button>

      <!-- ML Pipelines (Lucide `brain-circuit`) -->
      <button
        v-if="showMlPanel"
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.activePanel === 'ml' }"
        title="ML Pipelines"
        @click="toggle('ml')"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18Z" />
          <path d="M9 13a4.5 4.5 0 0 0 3-4" />
          <path d="M6.003 5.125A3 3 0 0 0 6.401 6.5" />
          <path d="M3.477 10.896a4 4 0 0 1 .585-.396" />
          <path d="M6 18a4 4 0 0 1-1.967-.516" />
          <path d="M12 13h4" />
          <path d="M12 18h6a2 2 0 0 1 2 2v1" />
          <path d="M12 8h8" />
          <path d="M16 8V5a2 2 0 0 1 2-2" />
          <circle cx="16" cy="13" r=".5" />
          <circle cx="18" cy="3" r=".5" />
          <circle cx="20" cy="21" r=".5" />
          <circle cx="20" cy="8" r=".5" />
        </svg>
      </button>

      <!-- Cypher Reference -->
      <button
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.activePanel === 'cypher-ref' }"
        title="Cypher Reference"
        @click="toggle('cypher-ref')"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20" />
          <path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z" />
        </svg>
      </button>

      <!-- Reference -->
      <button
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.activePanel === 'docs' }"
        title="Reference"
        @click="toggle('docs')"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="4 17 10 11 4 5" />
          <line x1="12" x2="20" y1="19" y2="19" />
        </svg>
      </button>

      <!-- Settings -->
      <button
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.activePanel === 'settings' }"
        title="Settings"
        @click="toggle('settings')"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
          <circle cx="12" cy="12" r="3" />
        </svg>
      </button>
    </div>

    <!-- Bottom actions -->
    <div class="activity-bar__bottom">
      <!-- Graph Assistant - shown when the backend can answer
           (`conn.aiChatAvailable`). -->
      <button
        v-if="showAiChat"
        class="activity-bar__btn"
        :class="{ 'activity-bar__btn--active': settings.aiChatOpen }"
        title="Graph Assistant"
        @click="settings.aiChatOpen = !settings.aiChatOpen"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M20.341 6.484A10 10 0 0 1 10.266 21.85" />
          <path d="M3.659 17.516A10 10 0 0 1 13.74 2.152" />
          <circle cx="12" cy="12" r="3" />
          <circle cx="19" cy="5" r="2" />
          <circle cx="5" cy="19" r="2" />
        </svg>
      </button>
    </div>
  </nav>
</template>

<style scoped>
.activity-bar {
  display: flex;
  flex-direction: column;
  width: var(--tv-activitybar-width);
  height: 100%;
  background: var(--tv-bg-surface);
  border-right: 1px solid var(--tv-border-default);
  flex-shrink: 0;
  box-sizing: content-box;
}

.activity-bar__logo {
  display: flex;
  align-items: center;
  justify-content: center;
  height: var(--tv-header-height);
  border: none;
  border-bottom: 1px solid var(--tv-border-default);
  background: transparent;
  flex-shrink: 0;
  cursor: pointer;
  transition: background 0.15s;
}
.activity-bar__logo:hover {
  background: var(--tv-bg-hover);
}
.activity-bar__logo--active {
  background: var(--tv-accent-subtle);
}

.activity-bar__logo-icon {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border-radius: var(--tv-radius-sm);
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
}
.activity-bar__logo-icon svg {
  width: 18px;
  height: 18px;
}

.activity-bar__icons {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 2px;
  padding: 8px 0;
}

.activity-bar__btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 40px;
  height: 40px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: transparent;
  color: var(--tv-text-muted);
  cursor: pointer;
  transition: color 0.15s, background 0.15s;
  position: relative;
}

.activity-bar__btn:hover {
  color: var(--tv-text-primary);
  background: var(--tv-bg-hover);
}

.activity-bar__btn--active {
  color: var(--tv-text-primary);
  background: var(--tv-accent-subtle);
}

.activity-bar__btn--active::before {
  content: '';
  position: absolute;
  left: 0;
  top: 6px;
  bottom: 6px;
  width: 2px;
  background: var(--tv-accent);
  border-radius: 0 1px 1px 0;
}

.activity-bar__bottom {
  margin-top: auto;
  display: flex;
  flex-direction: column;
  align-items: center;
  padding-bottom: 8px;
}

/* ---------- Mobile: bottom tab bar ---------- */
@media (max-width: 768px) {
  .activity-bar {
    flex-direction: row;
    width: 100%;
    height: var(--tv-activitybar-mobile-height, 56px);
    border-right: none;
    border-top: 1px solid var(--tv-border-default);
    padding: 0 4px;
    /* iOS safe-area bottom inset so buttons clear the home indicator. */
    padding-bottom: env(safe-area-inset-bottom);
    height: calc(var(--tv-activitybar-mobile-height, 56px) + env(safe-area-inset-bottom));
    /* When the soft keyboard is up, slide the bar above it so the
       tab buttons remain reachable instead of hiding under the
       keyboard. `--tv-keyboard-inset` is set by main.ts using the
       visual viewport API. */
    transform: translateY(calc(-1 * var(--tv-keyboard-inset, 0px)));
    transition: transform 0.15s ease;
    align-items: center;
    overflow-x: auto;
    overflow-y: hidden;
    -webkit-overflow-scrolling: touch;
    /* Hide the scrollbar - on touch the bar is swipeable, and a
       visible scrollbar would draw a line right above the safe-area
       padding. */
    scrollbar-width: none;
  }
  .activity-bar::-webkit-scrollbar {
    display: none;
  }

  /* On mobile the logo button doubles as the dashboard tab - reshape it
     to match the other icon buttons so it sits naturally in the row. */
  .activity-bar__logo {
    width: 44px;
    height: 44px;
    border-bottom: none;
    flex-shrink: 0;
  }
  .activity-bar__logo-icon {
    width: 24px;
    height: 24px;
  }
  .activity-bar__logo-icon svg {
    width: 16px;
    height: 16px;
  }

  .activity-bar__icons {
    flex-direction: row;
    padding: 0;
    gap: 2px;
    flex: 1;
    justify-content: flex-start;
  }

  /* Bottom group (chat) sits on the right side of the bar. */
  .activity-bar__bottom {
    margin-top: 0;
    margin-left: auto;
    flex-direction: row;
    padding-bottom: 0;
    align-items: center;
  }

  .activity-bar__btn {
    width: 44px;
    height: 44px;
    flex-shrink: 0;
  }

  /* Active indicator at top edge instead of left edge. */
  .activity-bar__btn--active::before {
    left: 6px;
    right: 6px;
    top: 0;
    bottom: auto;
    width: auto;
    height: 2px;
    border-radius: 0 0 1px 1px;
  }
}
</style>
