export interface EngineResult {
  qps: number
  p99Ms: number
}

export interface BenchmarkQuery {
  id: string
  name: string
  traverse: EngineResult
  memgraph: EngineResult | null
  neo4j: EngineResult
}

export interface ConcurrencyLevel {
  concurrency: number
  queries: BenchmarkQuery[]
  summary: {
    tvWinsMg: number; mgWinsTv: number; geoMeanMg: number
    tvWinsNj: number; njWinsTv: number; geoMeanNj: number
  }
}

export interface BenchmarkCategory {
  id: string
  label: string
  queryIds: string[]
}

export const benchmarkMeta = {
  dataset: 'pokec_medium',
  nodes: 100_000,
  edges: 1_770_000,
  date: '2026-07-06',
  traverseVersion: '4ff407e',
  memgraphVersion: '3.8.1',
  neo4jVersion: '2026.01.4',
}

export const benchmarkCategories: BenchmarkCategory[] = [
  { id: 'read', label: 'Read', queryIds: ['Q1', 'Q2', 'Q3', 'Q4', 'Q5'] },
  { id: 'traversal', label: 'Traversal', queryIds: ['Q6', 'Q7', 'Q8', 'Q9', 'Q10', 'Q11', 'Q12', 'Q13', 'Q14', 'Q15', 'Q16', 'Q17'] },
  { id: 'scan', label: 'Scan', queryIds: ['Q18', 'Q19', 'Q20', 'Q21', 'Q22'] },
  { id: 'aggregate', label: 'Aggregate', queryIds: ['Q23', 'Q24', 'Q25', 'Q26', 'Q27', 'Q28'] },
  { id: 'path', label: 'Shortest Path', queryIds: ['Q29', 'Q30'] },
  { id: 'write', label: 'Write', queryIds: ['Q31', 'Q32', 'Q33'] },
]

// -- Concurrency 1 --------------------------------------------------

const c1Queries: BenchmarkQuery[] = [
  { id: 'Q1', name: 'single_vertex_read', traverse: { qps: 56288, p99Ms: 0.02 }, memgraph: { qps: 32025, p99Ms: 0.04 }, neo4j: { qps: 16384, p99Ms: 0.31 } },
  { id: 'Q2', name: 'single_vertex_property', traverse: { qps: 55930, p99Ms: 0.02 }, memgraph: { qps: 31713, p99Ms: 0.04 }, neo4j: { qps: 18889, p99Ms: 0.15 } },
  { id: 'Q3', name: 'pattern_short', traverse: { qps: 53513, p99Ms: 0.02 }, memgraph: { qps: 28759, p99Ms: 0.04 }, neo4j: { qps: 18511, p99Ms: 0.11 } },
  { id: 'Q4', name: 'pattern_cycle', traverse: { qps: 27422, p99Ms: 0.13 }, memgraph: { qps: 18613, p99Ms: 0.16 }, neo4j: { qps: 10815, p99Ms: 0.28 } },
  { id: 'Q5', name: 'pattern_long', traverse: { qps: 47232, p99Ms: 0.04 }, memgraph: { qps: 25500, p99Ms: 0.07 }, neo4j: { qps: 18141, p99Ms: 0.10 } },
  { id: 'Q6', name: 'expansion_1', traverse: { qps: 43653, p99Ms: 0.05 }, memgraph: { qps: 26059, p99Ms: 0.07 }, neo4j: { qps: 13418, p99Ms: 0.19 } },
  { id: 'Q7', name: 'expansion_1_with_filter', traverse: { qps: 40533, p99Ms: 0.05 }, memgraph: { qps: 25289, p99Ms: 0.07 }, neo4j: { qps: 14282, p99Ms: 0.16 } },
  { id: 'Q8', name: 'expansion_2', traverse: { qps: 4785, p99Ms: 1.49 }, memgraph: { qps: 2873, p99Ms: 2.48 }, neo4j: { qps: 965.5, p99Ms: 6.37 } },
  { id: 'Q9', name: 'expansion_2_with_filter', traverse: { qps: 3672, p99Ms: 1.90 }, memgraph: { qps: 3048, p99Ms: 2.17 }, neo4j: { qps: 1296, p99Ms: 4.45 } },
  { id: 'Q10', name: 'expansion_3', traverse: { qps: 323, p99Ms: 11.97 }, memgraph: { qps: 104.2, p99Ms: 65.4 }, neo4j: { qps: 61.4, p99Ms: 118.8 } },
  { id: 'Q11', name: 'expansion_3_with_filter', traverse: { qps: 250, p99Ms: 15.14 }, memgraph: { qps: 105.9, p99Ms: 73.0 }, neo4j: { qps: 61.5, p99Ms: 109.7 } },
  { id: 'Q12', name: 'expansion_4', traverse: { qps: 64, p99Ms: 32.33 }, memgraph: { qps: 2.63, p99Ms: 2380 }, neo4j: { qps: 0.17, p99Ms: 6240 } },
  { id: 'Q13', name: 'expansion_4_with_filter', traverse: { qps: 53, p99Ms: 37.91 }, memgraph: { qps: 1.31, p99Ms: 4910 }, neo4j: { qps: 3.03, p99Ms: 971.8 } },
  { id: 'Q14', name: 'neighbours_2', traverse: { qps: 4601, p99Ms: 1.55 }, memgraph: { qps: 2734, p99Ms: 2.62 }, neo4j: { qps: 942.3, p99Ms: 6.55 } },
  { id: 'Q15', name: 'neighbours_2_with_filter', traverse: { qps: 3519, p99Ms: 2.24 }, memgraph: { qps: 2931, p99Ms: 2.22 }, neo4j: { qps: 1260, p99Ms: 4.63 } },
  { id: 'Q16', name: 'neighbours_2_with_data', traverse: { qps: 2833, p99Ms: 2.62 }, memgraph: { qps: 1531, p99Ms: 4.92 }, neo4j: { qps: 905.9, p99Ms: 6.55 } },
  { id: 'Q17', name: 'neighbours_2_with_data_and_filter', traverse: { qps: 2087, p99Ms: 4.18 }, memgraph: { qps: 1517, p99Ms: 5.10 }, neo4j: { qps: 1160, p99Ms: 6.58 } },
  { id: 'Q18', name: 'scan_expand', traverse: { qps: 4, p99Ms: 232.55 }, memgraph: { qps: 1.96, p99Ms: 525.4 }, neo4j: { qps: 0.62, p99Ms: 1640 } },
  { id: 'Q19', name: 'scan_expand_filter', traverse: { qps: 2791, p99Ms: 0.41 }, memgraph: { qps: 2090, p99Ms: 0.52 }, neo4j: { qps: 597.9, p99Ms: 2.11 } },
  { id: 'Q20', name: 'scan_expand_2hop', traverse: { qps: 2, p99Ms: 517.05 }, memgraph: { qps: 0.04, p99Ms: 25670 }, neo4j: { qps: 0.16, p99Ms: 6420 } },
  { id: 'Q21', name: 'scan_expand_2hop_filter', traverse: { qps: 147, p99Ms: 7.92 }, memgraph: { qps: 49.8, p99Ms: 21.9 }, neo4j: { qps: 44.3, p99Ms: 28.2 } },
  { id: 'Q22', name: 'scan_project', traverse: { qps: 56, p99Ms: 19.17 }, memgraph: { qps: 27.1, p99Ms: 37.6 }, neo4j: { qps: 11.0, p99Ms: 95.7 } },
  { id: 'Q23', name: 'scan_agg_global', traverse: { qps: 15, p99Ms: 74.19 }, memgraph: { qps: 0.11, p99Ms: 9090 }, neo4j: { qps: 0.22, p99Ms: 4510 } },
  { id: 'Q24', name: 'scan_agg_grouped', traverse: { qps: 6, p99Ms: 188.82 }, memgraph: { qps: 0.06, p99Ms: 15950 }, neo4j: { qps: 0.15, p99Ms: 6550 } },
  { id: 'Q25', name: 'count', traverse: { qps: 1228, p99Ms: 0.95 }, memgraph: { qps: 116.6, p99Ms: 8.87 }, neo4j: { qps: 104.4, p99Ms: 17.5 } },
  { id: 'Q26', name: 'min_max_avg', traverse: { qps: 1035, p99Ms: 1.11 }, memgraph: { qps: 47.2, p99Ms: 21.9 }, neo4j: { qps: 79.0, p99Ms: 14.4 } },
  { id: 'Q27', name: 'group_by_age', traverse: { qps: 725, p99Ms: 1.59 }, memgraph: { qps: 87.2, p99Ms: 11.9 }, neo4j: { qps: 102.2, p99Ms: 12.2 } },
  { id: 'Q28', name: 'aggregate_with_filter', traverse: { qps: 101, p99Ms: 11.83 }, memgraph: { qps: 67.8, p99Ms: 16.1 }, neo4j: { qps: 278.1, p99Ms: 4.75 } },
  { id: 'Q29', name: 'shortest_path', traverse: { qps: 16240, p99Ms: 0.21 }, memgraph: null, neo4j: { qps: 2984, p99Ms: 1.40 } },
  { id: 'Q30', name: 'all_shortest_paths', traverse: { qps: 18197, p99Ms: 0.24 }, memgraph: null, neo4j: { qps: 13007, p99Ms: 0.16 } },
  { id: 'Q31', name: 'vertex_update', traverse: { qps: 53218, p99Ms: 0.02 }, memgraph: { qps: 29033, p99Ms: 0.04 }, neo4j: { qps: 410.7, p99Ms: 6.76 } },
  { id: 'Q32', name: 'vertex_create', traverse: { qps: 54574, p99Ms: 0.02 }, memgraph: { qps: 29049, p99Ms: 0.05 }, neo4j: { qps: 468.6, p99Ms: 6.39 } },
  { id: 'Q33', name: 'edge_create', traverse: { qps: 52189, p99Ms: 0.03 }, memgraph: { qps: 24910, p99Ms: 0.06 }, neo4j: { qps: 344.8, p99Ms: 9.04 } },
]

// -- Concurrency 12 -------------------------------------------------

const c12Queries: BenchmarkQuery[] = [
  { id: 'Q1', name: 'single_vertex_read', traverse: { qps: 262882, p99Ms: 0.16 }, memgraph: { qps: 177036, p99Ms: 0.15 }, neo4j: { qps: 83399, p99Ms: 0.62 } },
  { id: 'Q2', name: 'single_vertex_property', traverse: { qps: 266226, p99Ms: 0.15 }, memgraph: { qps: 174471, p99Ms: 0.15 }, neo4j: { qps: 90618, p99Ms: 0.34 } },
  { id: 'Q3', name: 'pattern_short', traverse: { qps: 256730, p99Ms: 0.15 }, memgraph: { qps: 164036, p99Ms: 0.15 }, neo4j: { qps: 91687, p99Ms: 0.24 } },
  { id: 'Q4', name: 'pattern_cycle', traverse: { qps: 178070, p99Ms: 0.24 }, memgraph: { qps: 118662, p99Ms: 0.30 }, neo4j: { qps: 58499, p99Ms: 0.55 } },
  { id: 'Q5', name: 'pattern_long', traverse: { qps: 244538, p99Ms: 0.16 }, memgraph: { qps: 147787, p99Ms: 0.18 }, neo4j: { qps: 86086, p99Ms: 0.23 } },
  { id: 'Q6', name: 'expansion_1', traverse: { qps: 239882, p99Ms: 0.18 }, memgraph: { qps: 149460, p99Ms: 0.18 }, neo4j: { qps: 71682, p99Ms: 0.34 } },
  { id: 'Q7', name: 'expansion_1_with_filter', traverse: { qps: 234213, p99Ms: 0.18 }, memgraph: { qps: 149526, p99Ms: 0.17 }, neo4j: { qps: 74845, p99Ms: 0.31 } },
  { id: 'Q8', name: 'expansion_2', traverse: { qps: 33283, p99Ms: 2.30 }, memgraph: { qps: 24856, p99Ms: 3.52 }, neo4j: { qps: 6614, p99Ms: 10.4 } },
  { id: 'Q9', name: 'expansion_2_with_filter', traverse: { qps: 27302, p99Ms: 3.00 }, memgraph: { qps: 27611, p99Ms: 2.76 }, neo4j: { qps: 9056, p99Ms: 7.41 } },
  { id: 'Q10', name: 'expansion_3', traverse: { qps: 1675, p99Ms: 25.07 }, memgraph: { qps: 681.4, p99Ms: 105.3 }, neo4j: { qps: 342.7, p99Ms: 206.8 } },
  { id: 'Q11', name: 'expansion_3_with_filter', traverse: { qps: 1716, p99Ms: 25.96 }, memgraph: { qps: 961.1, p99Ms: 90.5 }, neo4j: { qps: 406.5, p99Ms: 187.9 } },
  { id: 'Q12', name: 'expansion_4', traverse: { qps: 299, p99Ms: 83.43 }, memgraph: { qps: 7.95, p99Ms: 11510 }, neo4j: { qps: 4.29, p99Ms: 20140 } },
  { id: 'Q13', name: 'expansion_4_with_filter', traverse: { qps: 309, p99Ms: 79.75 }, memgraph: { qps: 6.73, p99Ms: 12320 }, neo4j: { qps: 19.6, p99Ms: 2050 } },
  { id: 'Q14', name: 'neighbours_2', traverse: { qps: 32370, p99Ms: 2.37 }, memgraph: { qps: 24125, p99Ms: 3.62 }, neo4j: { qps: 6639, p99Ms: 10.5 } },
  { id: 'Q15', name: 'neighbours_2_with_filter', traverse: { qps: 27068, p99Ms: 2.95 }, memgraph: { qps: 26826, p99Ms: 2.85 }, neo4j: { qps: 9488, p99Ms: 7.15 } },
  { id: 'Q16', name: 'neighbours_2_with_data', traverse: { qps: 18898, p99Ms: 4.31 }, memgraph: { qps: 13336, p99Ms: 7.35 }, neo4j: { qps: 5909, p99Ms: 11.6 } },
  { id: 'Q17', name: 'neighbours_2_with_data_and_filter', traverse: { qps: 15034, p99Ms: 5.68 }, memgraph: { qps: 10622, p99Ms: 8.36 }, neo4j: { qps: 7415, p99Ms: 11.1 } },
  { id: 'Q18', name: 'scan_expand', traverse: { qps: 8, p99Ms: 1667.25 }, memgraph: { qps: 19.1, p99Ms: 841.8 }, neo4j: { qps: 2.60, p99Ms: 5750 } },
  { id: 'Q19', name: 'scan_expand_filter', traverse: { qps: 22081, p99Ms: 0.74 }, memgraph: { qps: 17472, p99Ms: 0.98 }, neo4j: { qps: 3535, p99Ms: 4.88 } },
  { id: 'Q20', name: 'scan_expand_2hop', traverse: { qps: 3, p99Ms: 6457.95 }, memgraph: { qps: 0.27, p99Ms: 43860 }, neo4j: { qps: 0.85, p99Ms: 14050 } },
  { id: 'Q21', name: 'scan_expand_2hop_filter', traverse: { qps: 284, p99Ms: 62.80 }, memgraph: { qps: 418.2, p99Ms: 55.8 }, neo4j: { qps: 416.9, p99Ms: 55.4 } },
  { id: 'Q22', name: 'scan_project', traverse: { qps: 389, p99Ms: 41.33 }, memgraph: { qps: 267.1, p99Ms: 58.5 }, neo4j: { qps: 69.3, p99Ms: 255.6 } },
  { id: 'Q23', name: 'scan_agg_global', traverse: { qps: 100, p99Ms: 170.69 }, memgraph: { qps: 0.76, p99Ms: 15730 }, neo4j: { qps: 0.84, p99Ms: 14290 } },
  { id: 'Q24', name: 'scan_agg_grouped', traverse: { qps: 41, p99Ms: 385.20 }, memgraph: { qps: 0.53, p99Ms: 22540 }, neo4j: { qps: 0.79, p99Ms: 15020 } },
  { id: 'Q25', name: 'count', traverse: { qps: 6549, p99Ms: 3.38 }, memgraph: { qps: 1222, p99Ms: 14.2 }, neo4j: { qps: 786.7, p99Ms: 37.2 } },
  { id: 'Q26', name: 'min_max_avg', traverse: { qps: 5758, p99Ms: 3.80 }, memgraph: { qps: 479.4, p99Ms: 35.1 }, neo4j: { qps: 714.3, p99Ms: 31.3 } },
  { id: 'Q27', name: 'group_by_age', traverse: { qps: 5138, p99Ms: 3.38 }, memgraph: { qps: 891.2, p99Ms: 18.0 }, neo4j: { qps: 927.6, p99Ms: 22.4 } },
  { id: 'Q28', name: 'aggregate_with_filter', traverse: { qps: 751, p99Ms: 21.80 }, memgraph: { qps: 515.7, p99Ms: 27.8 }, neo4j: { qps: 2132, p99Ms: 9.23 } },
  { id: 'Q29', name: 'shortest_path', traverse: { qps: 122724, p99Ms: 0.30 }, memgraph: null, neo4j: { qps: 24374, p99Ms: 1.97 } },
  { id: 'Q30', name: 'all_shortest_paths', traverse: { qps: 129937, p99Ms: 0.40 }, memgraph: null, neo4j: { qps: 68433, p99Ms: 0.29 } },
  { id: 'Q31', name: 'vertex_update', traverse: { qps: 225628, p99Ms: 0.13 }, memgraph: { qps: 141755, p99Ms: 0.28 }, neo4j: { qps: 1491, p99Ms: 12.4 } },
  { id: 'Q32', name: 'vertex_create', traverse: { qps: 257311, p99Ms: 0.11 }, memgraph: { qps: 136338, p99Ms: 0.27 }, neo4j: { qps: 1824, p99Ms: 13.8 } },
  { id: 'Q33', name: 'edge_create', traverse: { qps: 213249, p99Ms: 0.14 }, memgraph: { qps: 130603, p99Ms: 0.25 }, neo4j: { qps: 1264, p99Ms: 18.1 } },
]

// -- Concurrency 24 -------------------------------------------------

const c24Queries: BenchmarkQuery[] = [
  { id: 'Q1', name: 'single_vertex_read', traverse: { qps: 280823, p99Ms: 0.26 }, memgraph: { qps: 150210, p99Ms: 0.37 }, neo4j: { qps: 89984, p99Ms: 1.55 } },
  { id: 'Q2', name: 'single_vertex_property', traverse: { qps: 270762, p99Ms: 0.27 }, memgraph: { qps: 150245, p99Ms: 0.37 }, neo4j: { qps: 103330, p99Ms: 0.74 } },
  { id: 'Q3', name: 'pattern_short', traverse: { qps: 262403, p99Ms: 0.26 }, memgraph: { qps: 127578, p99Ms: 0.53 }, neo4j: { qps: 107004, p99Ms: 0.57 } },
  { id: 'Q4', name: 'pattern_cycle', traverse: { qps: 178252, p99Ms: 0.40 }, memgraph: { qps: 82530, p99Ms: 1.05 }, neo4j: { qps: 68844, p99Ms: 1.09 } },
  { id: 'Q5', name: 'pattern_long', traverse: { qps: 247093, p99Ms: 0.31 }, memgraph: { qps: 102271, p99Ms: 0.84 }, neo4j: { qps: 102713, p99Ms: 0.55 } },
  { id: 'Q6', name: 'expansion_1', traverse: { qps: 243140, p99Ms: 0.33 }, memgraph: { qps: 115176, p99Ms: 0.69 }, neo4j: { qps: 84777, p99Ms: 0.70 } },
  { id: 'Q7', name: 'expansion_1_with_filter', traverse: { qps: 238758, p99Ms: 0.35 }, memgraph: { qps: 107511, p99Ms: 0.80 }, neo4j: { qps: 88548, p99Ms: 0.66 } },
  { id: 'Q8', name: 'expansion_2', traverse: { qps: 34643, p99Ms: 3.17 }, memgraph: { qps: 25528, p99Ms: 4.86 }, neo4j: { qps: 6514, p99Ms: 23.8 } },
  { id: 'Q9', name: 'expansion_2_with_filter', traverse: { qps: 30390, p99Ms: 3.95 }, memgraph: { qps: 29397, p99Ms: 4.04 }, neo4j: { qps: 10092, p99Ms: 13.9 } },
  { id: 'Q10', name: 'expansion_3', traverse: { qps: 1786, p99Ms: 39.91 }, memgraph: { qps: 794.9, p99Ms: 160.4 }, neo4j: { qps: 201.1, p99Ms: 429.7 } },
  { id: 'Q11', name: 'expansion_3_with_filter', traverse: { qps: 1949, p99Ms: 38.67 }, memgraph: { qps: 945.6, p99Ms: 140.9 }, neo4j: { qps: 413.3, p99Ms: 340.8 } },
  { id: 'Q12', name: 'expansion_4', traverse: { qps: 299, p99Ms: 174.86 }, memgraph: { qps: 8.62, p99Ms: 16540 }, neo4j: { qps: 5.10, p99Ms: 16170 } },
  { id: 'Q13', name: 'expansion_4_with_filter', traverse: { qps: 318, p99Ms: 160.76 }, memgraph: { qps: 8.53, p99Ms: 13470 }, neo4j: { qps: 19.1, p99Ms: 4410 } },
  { id: 'Q14', name: 'neighbours_2', traverse: { qps: 33709, p99Ms: 3.19 }, memgraph: { qps: 24332, p99Ms: 5.13 }, neo4j: { qps: 6735, p99Ms: 22.5 } },
  { id: 'Q15', name: 'neighbours_2_with_filter', traverse: { qps: 29879, p99Ms: 3.96 }, memgraph: { qps: 28290, p99Ms: 4.18 }, neo4j: { qps: 9993, p99Ms: 14.5 } },
  { id: 'Q16', name: 'neighbours_2_with_data', traverse: { qps: 19933, p99Ms: 5.85 }, memgraph: { qps: 14348, p99Ms: 10.4 }, neo4j: { qps: 5819, p99Ms: 27.4 } },
  { id: 'Q17', name: 'neighbours_2_with_data_and_filter', traverse: { qps: 16578, p99Ms: 7.73 }, memgraph: { qps: 9939, p99Ms: 16.0 }, neo4j: { qps: 7733, p99Ms: 19.5 } },
  { id: 'Q18', name: 'scan_expand', traverse: { qps: 10, p99Ms: 5148.98 }, memgraph: { qps: 21.5, p99Ms: 1790 }, neo4j: { qps: 2.35, p99Ms: 11640 } },
  { id: 'Q19', name: 'scan_expand_filter', traverse: { qps: 24244, p99Ms: 1.79 }, memgraph: { qps: 18276, p99Ms: 2.84 }, neo4j: { qps: 3586, p99Ms: 15.1 } },
  { id: 'Q20', name: 'scan_expand_2hop', traverse: { qps: 3, p99Ms: 8789.43 }, memgraph: { qps: 0.28, p99Ms: 84740 }, neo4j: { qps: 0.86, p99Ms: 27840 } },
  { id: 'Q21', name: 'scan_expand_2hop_filter', traverse: { qps: 283, p99Ms: 141.48 }, memgraph: { qps: 469.1, p99Ms: 77.6 }, neo4j: { qps: 394.0, p99Ms: 126.1 } },
  { id: 'Q22', name: 'scan_project', traverse: { qps: 420, p99Ms: 83.01 }, memgraph: { qps: 272.4, p99Ms: 119.3 }, neo4j: { qps: 59.2, p99Ms: 725.5 } },
  { id: 'Q23', name: 'scan_agg_global', traverse: { qps: 98, p99Ms: 388.33 }, memgraph: { qps: 0.92, p99Ms: 26170 }, neo4j: { qps: 0.76, p99Ms: 31630 } },
  { id: 'Q24', name: 'scan_agg_grouped', traverse: { qps: 44, p99Ms: 820.80 }, memgraph: { qps: 0.54, p99Ms: 44360 }, neo4j: { qps: 0.85, p99Ms: 28140 } },
  { id: 'Q25', name: 'count', traverse: { qps: 6875, p99Ms: 7.14 }, memgraph: { qps: 1308, p99Ms: 35.4 }, neo4j: { qps: 727.4, p99Ms: 71.2 } },
  { id: 'Q26', name: 'min_max_avg', traverse: { qps: 6053, p99Ms: 7.77 }, memgraph: { qps: 554.6, p99Ms: 68.3 }, neo4j: { qps: 619.1, p99Ms: 68.3 } },
  { id: 'Q27', name: 'group_by_age', traverse: { qps: 5481, p99Ms: 8.04 }, memgraph: { qps: 878.1, p99Ms: 40.2 }, neo4j: { qps: 819.9, p99Ms: 53.4 } },
  { id: 'Q28', name: 'aggregate_with_filter', traverse: { qps: 777, p99Ms: 53.27 }, memgraph: { qps: 710.0, p99Ms: 61.4 }, neo4j: { qps: 2378, p99Ms: 22.4 } },
  { id: 'Q29', name: 'shortest_path', traverse: { qps: 120151, p99Ms: 0.52 }, memgraph: null, neo4j: { qps: 28216, p99Ms: 3.45 } },
  { id: 'Q30', name: 'all_shortest_paths', traverse: { qps: 125064, p99Ms: 0.59 }, memgraph: null, neo4j: { qps: 83208, p99Ms: 0.75 } },
  { id: 'Q31', name: 'vertex_update', traverse: { qps: 238811, p99Ms: 0.25 }, memgraph: { qps: 88176, p99Ms: 1.08 }, neo4j: { qps: 1881, p99Ms: 22.0 } },
  { id: 'Q32', name: 'vertex_create', traverse: { qps: 276375, p99Ms: 0.21 }, memgraph: { qps: 86066, p99Ms: 1.14 }, neo4j: { qps: 2739, p99Ms: 15.2 } },
  { id: 'Q33', name: 'edge_create', traverse: { qps: 220811, p99Ms: 0.26 }, memgraph: { qps: 84861, p99Ms: 1.18 }, neo4j: { qps: 1850, p99Ms: 21.9 } },
]

export const concurrencyLevels: ConcurrencyLevel[] = [
  {
    concurrency: 1,
    queries: c1Queries,
    summary: { tvWinsMg: 31, mgWinsTv: 0, geoMeanMg: 3.77, tvWinsNj: 32, njWinsTv: 1, geoMeanNj: 7.31 },
  },
  {
    concurrency: 12,
    queries: c12Queries,
    summary: { tvWinsMg: 28, mgWinsTv: 3, geoMeanMg: 2.82, tvWinsNj: 31, njWinsTv: 2, geoMeanNj: 6.45 },
  },
  {
    concurrency: 24,
    queries: c24Queries,
    summary: { tvWinsMg: 29, mgWinsTv: 2, geoMeanMg: 3.13, tvWinsNj: 31, njWinsTv: 2, geoMeanNj: 6.43 },
  },
]
