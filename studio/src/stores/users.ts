import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api, type UserInfo } from '../utils/api'

export const useUsersStore = defineStore('users', () => {
  const users = ref<UserInfo[]>([])
  const isLoading = ref(false)
  const error = ref<string | null>(null)

  async function fetchUsers() {
    isLoading.value = true
    error.value = null
    try {
      users.value = await api.users()
    } catch (e: any) {
      error.value = e.message || 'Failed to load users'
    } finally {
      isLoading.value = false
    }
  }

  async function createUser(username: string, password: string, role: string) {
    error.value = null
    try {
      await api.createUser(username, password, role)
      await fetchUsers()
    } catch (e: any) {
      error.value = e.message || 'Failed to create user'
      throw e
    }
  }

  async function updateUser(username: string, updates: { password?: string; role?: string }) {
    error.value = null
    try {
      await api.updateUser(username, updates)
      await fetchUsers()
    } catch (e: any) {
      error.value = e.message || 'Failed to update user'
      throw e
    }
  }

  async function deleteUser(username: string) {
    error.value = null
    try {
      await api.deleteUser(username)
      await fetchUsers()
    } catch (e: any) {
      error.value = e.message || 'Failed to delete user'
      throw e
    }
  }

  return {
    users,
    isLoading,
    error,
    fetchUsers,
    createUser,
    updateUser,
    deleteUser,
  }
})
