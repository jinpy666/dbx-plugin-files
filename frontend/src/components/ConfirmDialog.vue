<script setup lang="ts">
defineProps<{
  open: boolean;
  title: string;
  body?: string;
  danger?: boolean;
  dangerList?: string[];
  confirmLabel: string;
  cancelLabel: string;
  busy?: boolean;
  /** 有 slot 内容（表单）时确认按钮的可用性由父组件控制。 */
  confirmDisabled?: boolean;
}>();

const emit = defineEmits<{
  (event: "confirm"): void;
  (event: "cancel"): void;
}>();
</script>

<template>
  <div v-if="open" class="wb-dialog-backdrop" @click.self="emit('cancel')">
    <div class="wb-dialog" :class="{ 'is-danger': danger }" role="dialog" aria-modal="true">
      <header>{{ title }}</header>
      <div class="wb-dialog-body">
        <p v-if="body" style="margin: 0 0 6px">{{ body }}</p>
        <slot />
        <ul v-if="dangerList?.length" class="wb-dialog-danger-list">
          <li v-for="hit in dangerList" :key="hit">{{ hit }}</li>
        </ul>
      </div>
      <footer>
        <button type="button" class="wb-dialog-cancel" :disabled="busy" @click="emit('cancel')">{{ cancelLabel }}</button>
        <button type="button" :class="danger ? 'wb-dialog-danger' : 'wb-dialog-primary'" :disabled="busy || confirmDisabled" @click="emit('confirm')">
          {{ confirmLabel }}
        </button>
      </footer>
    </div>
  </div>
</template>
