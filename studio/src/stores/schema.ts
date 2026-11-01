import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api, type IndexInfo, type ConstraintInfo, type PropertyKeyInfo, type LabelDetail, type RelationshipTypeDetail } from '../utils/api'
import { useConnectionStore } from './connection'

export const useSchemaStore = defineStore('schema', () => {
  const labels = ref<string[]>([])
  const relationshipTypes = ref<string[]>([])
  const propertyKeys = ref<PropertyKeyInfo[]>([])
  const indexes = ref<IndexInfo[]>([])
  const constraints = ref<ConstraintInfo[]>([])
  const labelsDetail = ref<LabelDetail[]>([])
  const relationshipTypesDetail = ref<RelationshipTypeDetail[]>([])
  const isLoading = ref(false)

  async function fetchSchema() {
    const conn = useConnectionStore()
    isLoading.value = true
    try {
      const data = await api.schema(conn.currentDatabase)
      labels.value = data.labels
      relationshipTypes.value = data.relationship_types
      propertyKeys.value = data.property_keys
      indexes.value = data.indexes
      constraints.value = data.constraints ?? []
      labelsDetail.value = data.labels_detail ?? data.labels.map(name => ({ name, properties: [] }))
      relationshipTypesDetail.value = data.relationship_types_detail ?? data.relationship_types.map(name => ({ name, properties: [] }))
    } catch {
      // ignore - schema unavailable
    } finally {
      isLoading.value = false
    }
  }

  return { labels, relationshipTypes, propertyKeys, indexes, constraints, labelsDetail, relationshipTypesDetail, isLoading, fetchSchema }
})
