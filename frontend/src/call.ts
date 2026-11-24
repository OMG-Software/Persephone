import { mount } from 'svelte';
import CallWindow from './CallWindow.svelte';
import './style.css';

// The call window's page (ADR-0060). It has its own capability: the call
// commands only.
const target = document.getElementById('app');

if (!target) {
  throw new Error('Missing call window mount point');
}

mount(CallWindow, { target });
