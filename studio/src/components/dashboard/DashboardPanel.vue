<script setup lang="ts">
import { ref, computed, watch, onMounted, nextTick, watchEffect } from 'vue'
import { useSettingsStore } from '../../stores/settings'
import type { PanelId } from '../../stores/settings'
import {
  benchmarkMeta,
  benchmarkCategories,
  concurrencyLevels,
  type BenchmarkQuery,
} from '../../data/benchmark-data'
import {
  Chart as ChartJS,
  CategoryScale,
  LinearScale,
  LogarithmicScale,
  BarElement,
  Title,
  Tooltip,
  Legend,
} from 'chart.js'
import { Bar } from 'vue-chartjs'

ChartJS.register(CategoryScale, LinearScale, LogarithmicScale, BarElement, Title, Tooltip, Legend)

const settings = useSettingsStore()

function navigateTo(panel: PanelId) {
  settings.activePanel = panel
}

// Benchmark state
const showMethodology = ref(false)
const activeCategory = ref(benchmarkCategories[0]?.id ?? 'read')
const activeConcurrency = ref(1)
const selectedQueryId = ref<string | null>(null)

const activeConcLevel = computed(() =>
  concurrencyLevels.find(c => c.concurrency === activeConcurrency.value) ?? concurrencyLevels[0]
)

const activeBenchmarks = computed(() => {
  const cat = benchmarkCategories.find(c => c.id === activeCategory.value)
  if (!cat) return []
  return activeConcLevel.value.queries.filter(q => cat.queryIds.includes(q.id))
})

const maxQps = computed(() => {
  if (!activeBenchmarks.value.length) return 1
  return Math.max(
    ...activeBenchmarks.value.flatMap(q => {
      const vals = [q.traverse.qps, q.neo4j.qps]
      if (q.memgraph) vals.push(q.memgraph.qps)
      return vals
    })
  )
})

function barWidth(qps: number): string {
  return `${Math.max((qps / maxQps.value) * 100, 1)}%`
}

function formatQps(qps: number): string {
  if (qps >= 1_000_000) return `${(qps / 1_000_000).toFixed(1)}M`
  if (qps >= 1_000) return `${(qps / 1_000).toFixed(1)}K`
  if (qps >= 1) return String(Math.round(qps))
  return qps.toFixed(2)
}

function speedupRatio(a: number, b: number): string {
  if (!b) return '-'
  const r = a / b
  if (r >= 10) return `${Math.round(r)}x`
  return `${r.toFixed(1)}x`
}

function humanizeName(name: string): string {
  return name.replace(/_/g, ' ').replace(/\b\w/g, c => c.toUpperCase())
}

function isFast(q: BenchmarkQuery): boolean {
  const tv = q.traverse.qps
  const mg = q.memgraph?.qps ?? 0
  const nj = q.neo4j.qps
  return tv > mg * 1.1 && tv > nj * 1.1
}

function selectQuery(id: string) {
  selectedQueryId.value = selectedQueryId.value === id ? null : id
}

// The query shown in the scaling chart
const scalingQuery = computed(() => {
  if (selectedQueryId.value) {
    const q = activeConcLevel.value.queries.find(q => q.id === selectedQueryId.value)
    if (q) return q
  }
  const cat = benchmarkCategories.find(c => c.id === activeCategory.value)
  if (!cat || !cat.queryIds.length) return null
  return activeConcLevel.value.queries.find(q => q.id === cat.queryIds[0]) ?? null
})

// Chart.js data for the scaling chart
const chartKey = ref(0)

function getCssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim()
}

const chartData = computed(() => {
  void chartKey.value // recompute on theme change
  const qId = scalingQuery.value?.id
  if (!qId) return { labels: [], datasets: [] }

  const labels = concurrencyLevels.map(c => c.concurrency === 1 ? '1 client' : `${c.concurrency} clients`)
  const tvData: number[] = []
  const mgData: (number | null)[] = []
  const njData: number[] = []

  for (const cl of concurrencyLevels) {
    const q = cl.queries.find(q => q.id === qId)
    tvData.push(q?.traverse.qps ?? 0)
    mgData.push(q?.memgraph?.qps ?? null)
    njData.push(q?.neo4j.qps ?? 0)
  }

  return {
    labels,
    datasets: [
      {
        label: 'Traverse',
        data: tvData,
        backgroundColor: getCssVar('--tv-accent') || '#0EA5E9',
        borderRadius: 3,
        barPercentage: 0.95,
        categoryPercentage: 0.8,
      },
      {
        label: 'Memgraph',
        data: mgData,
        backgroundColor: getCssVar('--tv-status-success') || '#22C55E',
        borderRadius: 3,
        barPercentage: 0.95,
        categoryPercentage: 0.8,
      },
      {
        label: 'Neo4j',
        data: njData,
        backgroundColor: getCssVar('--tv-node-3') || '#E8716E',
        borderRadius: 3,
        barPercentage: 0.95,
        categoryPercentage: 0.8,
      },
    ],
  }
})

// Determine if we need log scale (range > 100x between min and max)
const useLogScale = computed(() => {
  const qId = scalingQuery.value?.id
  if (!qId) return false
  const vals: number[] = []
  for (const cl of concurrencyLevels) {
    const q = cl.queries.find(q => q.id === qId)
    if (q) {
      vals.push(q.traverse.qps)
      if (q.memgraph) vals.push(q.memgraph.qps)
      vals.push(q.neo4j.qps)
    }
  }
  const positive = vals.filter(v => v > 0)
  if (positive.length < 2) return false
  return Math.max(...positive) / Math.min(...positive) > 100
})

const chartOptions = computed(() => {
  void chartKey.value // recompute on theme change
  const textColor = getCssVar('--tv-text-muted') || '#888'
  const gridColor = getCssVar('--tv-border-subtle') || 'rgba(128,128,128,0.15)'

  return {
    responsive: true,
    maintainAspectRatio: false,
    animation: false as const,
    layout: { padding: { top: 8 } },
    plugins: {
      legend: { display: false },
      tooltip: {
        backgroundColor: getCssVar('--tv-bg-elevated') || '#1A2834',
        titleColor: getCssVar('--tv-text-primary') || '#ECE8E0',
        bodyColor: getCssVar('--tv-text-secondary') || '#7E8C9C',
        borderColor: getCssVar('--tv-border-default') || '#2A3A48',
        borderWidth: 1,
        padding: 10,
        cornerRadius: 6,
        callbacks: {
          label(ctx: any) {
            const v = ctx.parsed.y
            if (v == null) return `${ctx.dataset.label}: N/A`
            return `${ctx.dataset.label}: ${formatQps(v)} QPS`
          },
        },
      },
    },
    scales: {
      x: {
        ticks: { color: textColor, font: { size: 12 } },
        grid: { display: false },
        border: { display: false },
      },
      y: {
        type: 'linear' as const,
        beginAtZero: true,
        ticks: {
          color: textColor,
          font: { size: 11 },
          callback(value: any) { return formatQps(Number(value)) },
        },
        grid: { color: gridColor },
        border: { display: false },
        title: {
          display: true,
          text: 'Queries / sec (higher is better)',
          color: textColor,
          font: { size: 11 },
        },
      },
    },
  }
})

// Single custom plugin: bar labels + speedup annotations (afterDatasetsDraw fires before tooltip)
const chartOverlayPlugin = {
  id: 'chartOverlay',
  afterDatasetsDraw(chart: any) {
    const { ctx } = chart
    const tvMeta = chart.getDatasetMeta(0)
    if (!tvMeta.visible) return
    const inverseFill = getCssVar('--tv-text-inverse') || '#fff'
    const textColor = getCssVar('--tv-text-secondary') || '#888'

    // 1. Draw engine name inside each bar
    for (let dsIdx = 0; dsIdx < chart.data.datasets.length; dsIdx++) {
      const meta = chart.getDatasetMeta(dsIdx)
      if (!meta.visible) continue
      const ds = chart.data.datasets[dsIdx]

      for (let i = 0; i < meta.data.length; i++) {
        const el = meta.data[i]
        const val = ds.data[i] as number | null
        if (!val || val <= 0) continue

        const barHeight = el.base - el.y
        if (barHeight < 18) continue

        ctx.save()
        ctx.fillStyle = inverseFill
        ctx.font = 'bold 10px sans-serif'
        ctx.textAlign = 'center'
        ctx.textBaseline = 'top'
        ctx.fillText(ds.label, el.x, el.y + 4)
        ctx.restore()
      }
    }

    // 2. Draw speedup dashed lines + arrows for competitor bars
    for (let dsIdx = 1; dsIdx < chart.data.datasets.length; dsIdx++) {
      const meta = chart.getDatasetMeta(dsIdx)
      if (!meta.visible) continue

      for (let i = 0; i < meta.data.length; i++) {
        const tvEl = tvMeta.data[i]
        const el = meta.data[i]
        const tvVal = chart.data.datasets[0].data[i] as number
        const val = chart.data.datasets[dsIdx].data[i] as number | null
        if (!tvVal || !val || val <= 0) continue

        const ratio = tvVal / val
        if (ratio < 1.15) continue

        const yBot = el.y
        const yTop = tvEl.y
        const gap = yBot - yTop
        if (gap < 12) continue

        const x = el.x
        ctx.save()

        // Dashed vertical line
        ctx.setLineDash([4, 4])
        ctx.strokeStyle = textColor
        ctx.globalAlpha = 0.5
        ctx.lineWidth = 1.5
        ctx.beginPath()
        ctx.moveTo(x, yBot - 2)
        ctx.lineTo(x, yTop + 10)
        ctx.stroke()

        // Upward arrow
        ctx.setLineDash([])
        ctx.globalAlpha = 0.6
        ctx.fillStyle = textColor
        ctx.beginPath()
        ctx.moveTo(x, yTop + 3)
        ctx.lineTo(x - 4, yTop + 10)
        ctx.lineTo(x + 4, yTop + 10)
        ctx.closePath()
        ctx.fill()

        // Ratio label (right of the line)
        ctx.globalAlpha = 1
        ctx.fillStyle = textColor
        ctx.font = 'bold 13px monospace'
        ctx.textAlign = 'left'
        ctx.textBaseline = 'middle'
        const label = ratio >= 10 ? `${Math.round(ratio)}x` : `${ratio.toFixed(1)}x`
        ctx.fillText(label, x + 8, yTop + gap / 2)

        ctx.restore()
      }
    }
  },
}

const chartPlugins = [chartOverlayPlugin]

// Re-render chart on theme change
watch(() => settings.theme, () => {
  nextTick(() => { chartKey.value++ })
})

// Get Started cards
const getStartedCards = [
  {
    panel: 'examples' as PanelId,
    title: 'Load an Example',
    description: 'Pre-built datasets to explore',
    icon: 'examples',
  },
  {
    panel: 'import' as PanelId,
    title: 'Import Data',
    description: 'CSV, Cypher, or paste',
    icon: 'import',
  },
  {
    panel: 'cypher-ref' as PanelId,
    title: 'Cypher Reference',
    description: 'Browse all Cypher commands',
    icon: 'book',
  },
  {
    panel: 'docs' as PanelId,
    title: 'Reference Docs',
    description: 'Server, API, CLI, libraries',
    icon: 'terminal',
  },
  {
    panel: 'schema' as PanelId,
    title: 'Schema Browser',
    description: 'Explore labels, properties, indexes',
    icon: 'schema',
  },
  {
    panel: 'settings' as PanelId,
    title: 'Settings',
    description: 'Theme, editor, render limits',
    icon: 'settings',
  },
]

// Feature cards
const features = [
  {
    title: 'Pure In-Memory',
    description: 'Microsecond reads with zero disk I/O. Data is persisted to .tvdb files on graceful shutdown.',
    icon: 'memory',
  },
  {
    title: 'Full Cypher',
    description: 'All 3,893 openCypher TCK scenarios pass, and ISO/IEC 39075 GQL runs on the same engine.',
    icon: 'cypher',
  },
  {
    title: 'Bolt Protocol',
    description: 'Works with existing Bolt drivers and tools.',
    icon: 'bolt',
  },
  {
    title: 'Embeddable',
    description: 'Use as a library from .NET, Java, Python, Node.js, Go or C. No server process required.',
    icon: 'embed',
  },
  {
    title: 'Built-in Studio',
    description: 'Graph visualization, schema browser, Cypher editor with syntax highlighting.',
    icon: 'visual',
  },
  {
    title: 'Multi-Database',
    description: 'Multiple named databases, aliases, live rename and drop. Full isolation.',
    icon: 'multi',
  },
]
</script>

<template>
  <div class="dashboard">
    <div class="dashboard__scroll">
      <div class="dashboard__content">
        <!-- Hero -->
        <section class="dashboard__hero">
          <div class="dashboard__hero-logo">
            <svg viewBox="0 0 512 512" fill="none" xmlns="http://www.w3.org/2000/svg">
              <path d="M227.3 371.862V136.3H277.435L278.989 182.548L269.662 179.05C271.216 171.018 275.621 163.505 282.876 156.509C290.13 149.255 298.94 143.425 309.303 139.021C319.926 134.616 331.067 132.414 342.726 132.414C360.863 132.414 376.927 137.078 390.918 146.405C404.909 155.473 415.92 168.039 423.952 184.103C431.984 199.907 436 218.173 436 238.9C436 259.109 431.984 277.246 423.952 293.309C415.92 309.373 404.909 322.068 390.918 331.396C376.927 340.464 360.992 344.998 343.115 344.998C331.197 344.998 319.797 342.666 308.915 338.002C298.033 333.339 288.835 327.38 281.321 320.125C273.807 312.611 269.014 304.58 266.941 296.03L279.378 290.589V427L227.3 371.862ZM331.844 299.916C342.467 299.916 351.795 297.325 359.827 292.143C368.118 286.961 374.595 279.836 379.259 270.768C383.922 261.441 386.254 250.818 386.254 238.9C386.254 226.723 383.922 216.1 379.259 207.032C374.595 197.705 368.247 190.45 360.215 185.268C352.183 180.087 342.726 177.496 331.844 177.496C321.222 177.496 311.765 180.087 303.474 185.268C295.442 190.45 289.094 197.705 284.43 207.032C279.767 216.1 277.435 226.723 277.435 238.9C277.435 250.818 279.767 261.441 284.43 270.768C289.094 279.836 295.442 286.961 303.474 292.143C311.765 297.325 321.222 299.916 331.844 299.916Z" fill="currentColor"/>
              <path d="M115.253 342.277V213.638V85H166.942V213.638V342.277H115.253ZM76 184.102V137.077H210.47V184.102H76Z" fill="currentColor"/>
            </svg>
          </div>
          <h1 class="dashboard__hero-title">Traverse</h1>
          <p class="dashboard__hero-tagline">In-memory graph database for the modern data stack</p>

          <div class="dashboard__pills">
            <div class="dashboard__pill">
              <span class="dashboard__pill-value">7x</span>
              <span class="dashboard__pill-label">faster than Neo4j</span>
            </div>
            <div class="dashboard__pill">
              <span class="dashboard__pill-value">3.6x</span>
              <span class="dashboard__pill-label">faster than Memgraph</span>
            </div>
            <div class="dashboard__pill">
              <span class="dashboard__pill-value">4x</span>
              <span class="dashboard__pill-label">less memory than Neo4j</span>
            </div>
          </div>

          <a class="dashboard__docs-link" href="https://truespar.com/traverse/docs" target="_blank" rel="noopener">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
              <polyline points="15 3 21 3 21 9" />
              <line x1="10" x2="21" y1="14" y2="3" />
            </svg>
            Documentation
          </a>
        </section>

        <!-- Get Started -->
        <section class="dashboard__section">
          <h2 class="dashboard__section-title">Get Started</h2>
          <div class="dashboard__cards">
            <button
              v-for="card in getStartedCards"
              :key="card.title"
              class="dashboard__card"
              @click="navigateTo(card.panel)"
            >
              <div class="dashboard__card-icon">
                <!-- Examples (box) -->
                <svg v-if="card.icon === 'examples'" width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M12.89 1.45l8 4A2 2 0 0 1 22 7.24v9.53a2 2 0 0 1-1.11 1.79l-8 4a2 2 0 0 1-1.79 0l-8-4a2 2 0 0 1-1.1-1.8V7.24a2 2 0 0 1 1.11-1.79l8-4a2 2 0 0 1 1.78 0z" />
                  <polyline points="2.32 6.16 12 11 21.68 6.16" />
                  <line x1="12" x2="12" y1="22.76" y2="11" />
                </svg>
                <!-- Import (download) -->
                <svg v-else-if="card.icon === 'import'" width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M12 15V3" />
                  <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
                  <path d="m7 10 5 5 5-5" />
                </svg>
                <!-- Book -->
                <svg v-else-if="card.icon === 'book'" width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20" />
                  <path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z" />
                </svg>
                <!-- Terminal -->
                <svg v-else-if="card.icon === 'terminal'" width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <polyline points="4 17 10 11 4 5" />
                  <line x1="12" x2="20" y1="19" y2="19" />
                </svg>
                <!-- Schema (database) -->
                <svg v-else-if="card.icon === 'schema'" width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <ellipse cx="12" cy="5" rx="9" ry="3" />
                  <path d="M3 5V19A9 3 0 0 0 21 19V5" />
                  <path d="M3 12A9 3 0 0 0 21 12" />
                </svg>
                <!-- Settings (gear) -->
                <svg v-else-if="card.icon === 'settings'" width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
                  <circle cx="12" cy="12" r="3" />
                </svg>
              </div>
              <div class="dashboard__card-text">
                <span class="dashboard__card-title">{{ card.title }}</span>
                <span class="dashboard__card-desc">{{ card.description }}</span>
              </div>
            </button>
          </div>
        </section>

        <!-- Benchmark -->
        <section class="dashboard__section">
          <h2 class="dashboard__section-title">
            Benchmark
            <button class="dashboard__method-link" @click="showMethodology = true">Methodology</button>
          </h2>

          <div class="dashboard__bench-controls">
            <div class="dashboard__bench-tabs">
              <button
                v-for="cat in benchmarkCategories"
                :key="cat.id"
                class="dashboard__bench-tab"
                :class="{ 'dashboard__bench-tab--active': activeCategory === cat.id }"
                @click="activeCategory = cat.id"
              >{{ cat.label }}</button>
            </div>

            <div class="dashboard__bench-conc">
              <span class="dashboard__bench-conc-label">Parallel clients:</span>
              <div class="dashboard__bench-conc-pills">
                <button
                  v-for="cl in concurrencyLevels"
                  :key="cl.concurrency"
                  class="dashboard__bench-conc-pill"
                  :class="{ 'dashboard__bench-conc-pill--active': activeConcurrency === cl.concurrency }"
                  @click="activeConcurrency = cl.concurrency"
                >{{ cl.concurrency }}</button>
              </div>
            </div>
          </div>

          <div v-if="activeBenchmarks.length" class="dashboard__bench-rows">
            <div
              v-for="q in activeBenchmarks"
              :key="q.id"
              class="dashboard__bench-row"
              :class="{ 'dashboard__bench-row--selected': selectedQueryId === q.id }"
              @click="selectQuery(q.id)"
            >
              <span class="dashboard__bench-name">{{ humanizeName(q.name) }}</span>
              <div class="dashboard__bench-bars">
                <div class="dashboard__bench-bar-group">
                  <div class="dashboard__bench-bar-row">
                    <div class="dashboard__bench-bar dashboard__bench-bar--traverse" :style="{ width: barWidth(q.traverse.qps) }">
                      <span class="dashboard__bench-bar-label">Traverse</span>
                      <span class="dashboard__bench-qps">{{ formatQps(q.traverse.qps) }}</span>
                    </div>
                    <svg v-if="isFast(q)" class="dashboard__rabbit" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                      <path d="M13 16a3 3 0 0 1 2.24 5" /><path d="M18 12h.01" /><path d="M18 21h-8a4 4 0 0 1-4-4 7 7 0 0 1 7-7h.2L9.6 6.4a1 1 0 1 1 2.8-2.8L15.8 7h.2c3.3 0 6 2.7 6 6v1a2 2 0 0 1-2 2h-1a3 3 0 0 0-3 3" /><path d="M20 8.54V4a2 2 0 1 0-4 0v3" /><path d="M7.612 12.524a3 3 0 1 0-1.6 4.3" />
                    </svg>
                  </div>
                  <div v-if="q.memgraph" class="dashboard__bench-bar dashboard__bench-bar--memgraph" :style="{ width: barWidth(q.memgraph.qps) }">
                    <span class="dashboard__bench-bar-label">Memgraph</span>
                    <span class="dashboard__bench-qps">{{ formatQps(q.memgraph.qps) }}</span>
                  </div>
                  <div v-else class="dashboard__bench-bar-na">
                    <span class="dashboard__bench-qps-na">Memgraph &mdash;</span>
                  </div>
                  <div class="dashboard__bench-bar dashboard__bench-bar--neo4j" :style="{ width: barWidth(q.neo4j.qps) }">
                    <span class="dashboard__bench-bar-label">Neo4j</span>
                    <span class="dashboard__bench-qps">{{ formatQps(q.neo4j.qps) }}</span>
                  </div>
                </div>
                <div class="dashboard__bench-ratios">
                  <span v-if="q.memgraph" class="dashboard__bench-ratio">
                    <span class="dashboard__ratio-dot dashboard__ratio-dot--memgraph"></span>
                    {{ speedupRatio(q.traverse.qps, q.memgraph.qps) }}
                  </span>
                  <span v-else class="dashboard__bench-ratio dashboard__bench-ratio--na">&mdash;</span>
                  <span class="dashboard__bench-ratio">
                    <span class="dashboard__ratio-dot dashboard__ratio-dot--neo4j"></span>
                    {{ speedupRatio(q.traverse.qps, q.neo4j.qps) }}
                  </span>
                </div>
              </div>
            </div>

            <!-- Summary -->
            <div class="dashboard__bench-summary">
              <span>
                <span class="dashboard__ratio-dot dashboard__ratio-dot--memgraph"></span>
                Traverse wins {{ activeConcLevel.summary.tvWinsMg }}/{{ activeConcLevel.summary.tvWinsMg + activeConcLevel.summary.mgWinsTv }}
                vs Memgraph &middot; {{ activeConcLevel.summary.geoMeanMg }}x avg speedup
              </span>
              <span class="dashboard__bench-summary-sep">|</span>
              <span>
                <span class="dashboard__ratio-dot dashboard__ratio-dot--neo4j"></span>
                Traverse wins {{ activeConcLevel.summary.tvWinsNj }}/{{ activeConcLevel.summary.tvWinsNj + activeConcLevel.summary.njWinsTv }}
                vs Neo4j &middot; {{ activeConcLevel.summary.geoMeanNj }}x avg speedup
              </span>
            </div>
          </div>
          <div v-else class="dashboard__bench-empty">
            No benchmark data for this category yet.
          </div>

          <!-- Scaling chart -->
          <div v-if="scalingQuery" class="dashboard__chart-section">
            <h3 class="dashboard__chart-title">
              Throughput by concurrency: {{ humanizeName(scalingQuery.name) }}
            </h3>
            <div class="dashboard__chart-container">
              <Bar :key="chartKey" :data="chartData" :options="chartOptions" :plugins="chartPlugins" />
            </div>
          </div>
        </section>

        <!-- Why Traverse -->
        <section class="dashboard__section">
          <h2 class="dashboard__section-title">Why Traverse?</h2>
          <div class="dashboard__features">
            <div v-for="f in features" :key="f.title" class="dashboard__feature">
              <div class="dashboard__feature-icon">
                <!-- Memory (chip) -->
                <svg v-if="f.icon === 'memory'" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M6 19v2M10 19v2M14 19v2M18 19v2" /><rect x="4" y="6" width="16" height="13" rx="2" /><path d="M4 10h16" />
                </svg>
                <!-- Cypher (code) -->
                <svg v-else-if="f.icon === 'cypher'" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <polyline points="16 18 22 12 16 6" /><polyline points="8 6 2 12 8 18" />
                </svg>
                <!-- Bolt (zap) -->
                <svg v-else-if="f.icon === 'bolt'" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M13 2 3 14h9l-1 8 10-12h-9l1-8z" />
                </svg>
                <!-- Embed (puzzle) -->
                <svg v-else-if="f.icon === 'embed'" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M15.5 2H18a2 2 0 0 1 2 2v2.5" /><path d="M20 15.5V18a2 2 0 0 1-2 2h-2.5" /><path d="M8.5 22H6a2 2 0 0 1-2-2v-2.5" /><path d="M4 8.5V6a2 2 0 0 1 2-2h2.5" /><rect x="8" y="8" width="8" height="8" rx="1" />
                </svg>
                <!-- Visual (eye) -->
                <svg v-else-if="f.icon === 'visual'" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0" /><circle cx="12" cy="12" r="3" />
                </svg>
                <!-- Multi (layers) -->
                <svg v-else-if="f.icon === 'multi'" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                  <path d="m12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83Z" /><path d="m2 12 8.58 3.91a2 2 0 0 0 1.66 0L22 12" /><path d="m2 17 8.58 3.91a2 2 0 0 0 1.66 0L22 17" />
                </svg>
              </div>
              <div class="dashboard__feature-text">
                <span class="dashboard__feature-title">{{ f.title }}</span>
                <span class="dashboard__feature-desc">{{ f.description }}</span>
              </div>
            </div>
          </div>
        </section>
      </div>
    </div>

    <!-- Methodology modal -->
    <Teleport to="body">
      <div v-if="showMethodology" class="dashboard__modal-overlay" @click.self="showMethodology = false">
        <div class="dashboard__modal">
          <div class="dashboard__modal-header">
            <h3 class="dashboard__modal-title">Benchmark Methodology</h3>
            <button class="dashboard__modal-close" @click="showMethodology = false">
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" />
              </svg>
            </button>
          </div>
          <div class="dashboard__modal-body">
            <table class="dashboard__modal-table">
              <tbody>
                <tr><td>Dataset</td><td>Pokec Medium &mdash; {{ (benchmarkMeta.nodes / 1000).toFixed(0) }}K nodes, {{ (benchmarkMeta.edges / 1_000_000).toFixed(2) }}M edges</td></tr>
                <tr><td>Traverse</td><td>commit {{ benchmarkMeta.traverseVersion }}</td></tr>
                <tr><td>Memgraph</td><td>{{ benchmarkMeta.memgraphVersion }}</td></tr>
                <tr><td>Neo4j</td><td>{{ benchmarkMeta.neo4jVersion }}</td></tr>
                <tr><td>Date</td><td>{{ benchmarkMeta.date }}</td></tr>
                <tr><td>CPU</td><td>Intel Core i7-12700KF (12C/20T, P-cores 4.9 GHz)</td></tr>
                <tr><td>RAM</td><td>64 GB DDR5</td></tr>
                <tr><td>OS</td><td>Ubuntu 24.04 LTS, kernel 6.14</td></tr>
                <tr><td>CPU pinning</td><td><code>taskset -c 0-15</code> (P-cores only)</td></tr>
                <tr><td>Duration</td><td>10 seconds per query, 3 warmup iterations</td></tr>
                <tr><td>Concurrency</td><td>1, 12, and 24 concurrent clients</td></tr>
                <tr><td>Fresh data</td><td>Each engine reimported before every concurrency level</td></tr>
              </tbody>
            </table>
            <p class="dashboard__modal-note">
              All engines ran the same 33 Cypher queries via the Bolt protocol. Traverse and Memgraph used default
              configurations. Neo4j used <code>server.memory.heap.initial_size=4g</code> and
              <code>server.memory.heap.max_size=16g</code>. Results are deterministic within &plusmn;3% across runs.
            </p>
          </div>
        </div>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
.dashboard {
  width: 100%;
  height: 100%;
  overflow: hidden;
  background: var(--tv-bg-base);
}

.dashboard__scroll {
  height: 100%;
  overflow-y: auto;
  scrollbar-width: thin;
  scrollbar-color: var(--tv-border-default) transparent;
}

.dashboard__content {
  max-width: 960px;
  margin: 0 auto;
  padding: 48px 32px 64px;
}

/* Hero */
.dashboard__hero {
  display: flex;
  flex-direction: column;
  align-items: center;
  text-align: center;
  padding-bottom: 40px;
  border-bottom: 1px solid var(--tv-border-default);
  margin-bottom: 40px;
}

.dashboard__hero-logo {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 48px;
  height: 48px;
  border-radius: var(--tv-radius-lg);
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
  margin-bottom: 16px;
}
.dashboard__hero-logo svg {
  width: 30px;
  height: 30px;
}

.dashboard__hero-title {
  font-size: 24px;
  font-weight: 700;
  color: var(--tv-text-primary);
  margin: 0 0 6px;
  letter-spacing: -0.02em;
}

.dashboard__hero-tagline {
  font-size: 15px;
  color: var(--tv-text-secondary);
  margin: 0 0 24px;
}

.dashboard__pills {
  display: flex;
  gap: 12px;
  flex-wrap: wrap;
  justify-content: center;
}

.dashboard__pill {
  display: flex;
  align-items: baseline;
  gap: 6px;
  padding: 6px 14px;
  background: var(--tv-bg-elevated);
  border-radius: 20px;
  border: 1px solid var(--tv-border-default);
}

.dashboard__pill-value {
  font-size: 15px;
  font-weight: 700;
  color: var(--tv-accent);
  font-family: var(--tv-font-mono);
}

.dashboard__pill-label {
  font-size: 12px;
  color: var(--tv-text-muted);
}

.dashboard__docs-link {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  margin-top: 16px;
  font-size: 13px;
  color: var(--tv-text-muted);
  text-decoration: none;
  transition: color 0.15s;
}

.dashboard__docs-link:hover {
  color: var(--tv-accent);
}

/* Sections */
.dashboard__section {
  margin-bottom: 40px;
}

.dashboard__section-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.06em;
  margin: 0 0 16px;
}


/* Get Started Cards */
.dashboard__cards {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 10px;
}

.dashboard__card {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 16px;
  background: var(--tv-bg-surface);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s;
  text-align: left;
}
.dashboard__card:hover {
  background: var(--tv-bg-elevated);
  border-color: var(--tv-border-strong);
}

.dashboard__card-icon {
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  width: 36px;
  height: 36px;
  border-radius: var(--tv-radius-sm);
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
}

.dashboard__card-text {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.dashboard__card-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--tv-text-primary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.dashboard__card-desc {
  font-size: 12px;
  color: var(--tv-text-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* Benchmark section */
.dashboard__bench-controls {
  display: flex;
  flex-direction: column;
  gap: 10px;
  margin-bottom: 16px;
}

.dashboard__bench-tabs {
  display: flex;
  gap: 2px;
  background: var(--tv-bg-surface);
  border-radius: var(--tv-radius-md);
  padding: 3px;
  border: 1px solid var(--tv-border-default);
  width: fit-content;
}

.dashboard__bench-tab {
  padding: 5px 14px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: transparent;
  color: var(--tv-text-secondary);
  font-size: 12px;
  font-weight: 500;
  font-family: var(--tv-font-sans);
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.dashboard__bench-tab:hover {
  color: var(--tv-text-primary);
  background: var(--tv-bg-hover);
}
.dashboard__bench-tab--active {
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
}

.dashboard__bench-conc {
  display: flex;
  align-items: center;
  gap: 8px;
}

.dashboard__bench-conc-label {
  font-size: 12px;
  color: var(--tv-text-muted);
}

.dashboard__bench-conc-pills {
  display: flex;
  gap: 2px;
  background: var(--tv-bg-surface);
  border-radius: var(--tv-radius-md);
  padding: 3px;
  border: 1px solid var(--tv-border-default);
}

.dashboard__bench-conc-pill {
  padding: 4px 12px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: transparent;
  color: var(--tv-text-secondary);
  font-size: 12px;
  font-weight: 600;
  font-family: var(--tv-font-mono);
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.dashboard__bench-conc-pill:hover {
  color: var(--tv-text-primary);
  background: var(--tv-bg-hover);
}
.dashboard__bench-conc-pill--active {
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
}

.dashboard__bench-rows {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.dashboard__bench-row {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 6px 8px;
  border-radius: var(--tv-radius-sm);
  cursor: pointer;
  transition: background 0.1s;
}
.dashboard__bench-row:hover {
  background: var(--tv-bg-hover);
}
.dashboard__bench-row--selected {
  background: var(--tv-accent-subtle);
}

.dashboard__bench-name {
  font-size: 12px;
  font-weight: 500;
  color: var(--tv-text-secondary);
  font-family: var(--tv-font-mono);
}

.dashboard__bench-bars {
  display: flex;
  align-items: center;
  gap: 12px;
}

.dashboard__bench-bar-group {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.dashboard__bench-bar-row {
  display: flex;
  align-items: center;
  gap: 6px;
}

.dashboard__rabbit {
  flex-shrink: 0;
  color: var(--tv-accent);
  opacity: 0.7;
  animation: rabbit-bounce 1.2s ease-in-out infinite;
}

@keyframes rabbit-bounce {
  0%, 100% { transform: translateY(0); }
  50% { transform: translateY(-3px); }
}

.dashboard__bench-bar {
  height: 20px;
  border-radius: 3px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 8px;
  min-width: 80px;
  transition: width 0.3s ease;
  overflow: visible;
  position: relative;
}

.dashboard__bench-bar-label {
  font-size: 10px;
  font-weight: 600;
  color: var(--tv-text-inverse);
  font-family: var(--tv-font-sans);
  white-space: nowrap;
}

.dashboard__bench-bar--traverse {
  background: var(--tv-accent);
}

.dashboard__bench-bar--memgraph {
  background: var(--tv-status-success);
}

.dashboard__bench-bar--neo4j {
  background: var(--tv-node-3);
}

.dashboard__bench-bar-na {
  height: 18px;
  display: flex;
  align-items: center;
  padding: 0 8px;
}

.dashboard__bench-qps {
  font-size: 10px;
  font-weight: 600;
  color: var(--tv-text-inverse);
  font-family: var(--tv-font-mono);
  white-space: nowrap;
}

.dashboard__bench-qps-na {
  font-size: 10px;
  color: var(--tv-text-muted);
  font-family: var(--tv-font-mono);
}

.dashboard__bench-ratios {
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  align-items: flex-end;
  gap: 1px;
  min-width: 90px;
}

.dashboard__bench-ratio {
  display: flex;
  align-items: center;
  gap: 4px;
  font-size: 11px;
  font-weight: 600;
  color: var(--tv-accent);
  font-family: var(--tv-font-mono);
  white-space: nowrap;
}

.dashboard__bench-ratio--na {
  color: var(--tv-text-muted);
}

.dashboard__ratio-dot {
  display: inline-block;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex-shrink: 0;
  vertical-align: middle;
}
.dashboard__ratio-dot--memgraph {
  background: var(--tv-status-success);
}
.dashboard__ratio-dot--neo4j {
  background: var(--tv-node-3);
}

.dashboard__bench-summary {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  margin-top: 8px;
  padding: 8px 12px;
  background: var(--tv-bg-surface);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  font-size: 12px;
  color: var(--tv-text-secondary);
  font-family: var(--tv-font-mono);
}

.dashboard__bench-summary-sep {
  color: var(--tv-text-muted);
}

.dashboard__bench-empty {
  font-size: 13px;
  color: var(--tv-text-muted);
  padding: 24px;
  text-align: center;
  background: var(--tv-bg-surface);
  border-radius: var(--tv-radius-md);
  border: 1px solid var(--tv-border-default);
}

/* Chart section */
.dashboard__chart-section {
  margin-top: 24px;
}

.dashboard__chart-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--tv-text-secondary);
  margin: 0 0 8px;
  font-family: var(--tv-font-mono);
}

.dashboard__chart-container {
  height: 360px;
  background: var(--tv-bg-surface);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  padding: 16px;
}

/* Feature cards */
.dashboard__features {
  display: grid;
  grid-template-columns: repeat(2, 1fr);
  gap: 10px;
}

.dashboard__feature {
  display: flex;
  gap: 12px;
  padding: 14px 16px;
  background: var(--tv-bg-surface);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
}

.dashboard__feature-icon {
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  width: 36px;
  height: 36px;
  border-radius: var(--tv-radius-sm);
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
}

.dashboard__feature-text {
  display: flex;
  flex-direction: column;
  gap: 3px;
  min-width: 0;
}

.dashboard__feature-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--tv-text-primary);
}

.dashboard__feature-desc {
  font-size: 12px;
  color: var(--tv-text-muted);
  line-height: 1.5;
}

/* Methodology link */
.dashboard__method-link {
  background: none;
  border: none;
  padding: 0;
  font-size: var(--tv-font-size-xs);
  font-weight: 400;
  color: var(--tv-text-muted);
  cursor: pointer;
  text-decoration: underline;
  text-underline-offset: 2px;
  margin-left: 8px;
  vertical-align: middle;
}
.dashboard__method-link:hover {
  color: var(--tv-accent);
}

/* Methodology modal */
.dashboard__modal-overlay {
  position: fixed;
  inset: 0;
  z-index: 9999;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--tv-bg-overlay);
}

.dashboard__modal {
  width: 560px;
  max-width: 90vw;
  max-height: 80vh;
  background: var(--tv-bg-surface);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-lg);
  box-shadow: var(--tv-shadow-xl);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.dashboard__modal-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 16px 20px;
  border-bottom: 1px solid var(--tv-border-default);
}

.dashboard__modal-title {
  font-size: 15px;
  font-weight: 600;
  color: var(--tv-text-primary);
  margin: 0;
}

.dashboard__modal-close {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: transparent;
  color: var(--tv-text-muted);
  cursor: pointer;
}
.dashboard__modal-close:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}

.dashboard__modal-body {
  padding: 20px;
  overflow-y: auto;
}

.dashboard__modal-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 13px;
  margin-bottom: 16px;
}

.dashboard__modal-table td {
  padding: 6px 0;
  border-bottom: 1px solid var(--tv-border-subtle);
  color: var(--tv-text-secondary);
}

.dashboard__modal-table td:first-child {
  font-weight: 600;
  color: var(--tv-text-primary);
  width: 120px;
  white-space: nowrap;
  padding-right: 16px;
}

.dashboard__modal-table code {
  font-family: var(--tv-font-mono);
  font-size: 12px;
  padding: 1px 5px;
  background: var(--tv-bg-elevated);
  border-radius: 3px;
}

.dashboard__modal-note {
  font-size: 12px;
  color: var(--tv-text-muted);
  line-height: 1.6;
  margin: 0;
}

.dashboard__modal-note code {
  font-family: var(--tv-font-mono);
  font-size: 11px;
  padding: 1px 5px;
  background: var(--tv-bg-elevated);
  border-radius: 3px;
}

/* Responsive */
@media (max-width: 720px) {
  .dashboard__cards {
    grid-template-columns: repeat(2, 1fr);
  }
  .dashboard__features {
    grid-template-columns: 1fr;
  }
  .dashboard__content {
    padding: 32px 20px 48px;
  }
  .dashboard__bench-tabs {
    flex-wrap: wrap;
  }
}

@media (max-width: 480px) {
  .dashboard__cards {
    grid-template-columns: 1fr;
  }
  .dashboard__pills {
    flex-direction: column;
    align-items: center;
  }
  .dashboard__bench-summary {
    flex-direction: column;
    gap: 4px;
  }
  .dashboard__bench-summary-sep {
    display: none;
  }
}
</style>
