import { createApp } from 'vue'
import { createRouter, createWebHistory } from 'vue-router'
import App from './App.vue'
import './style.css'

/**
 * Routes are lazily imported so the initial load only ships the dashboard.
 * A server tool gets opened dozens of times a day; every kilobyte of startup
 * cost is paid repeatedly.
 */
const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/', name: 'dashboard', component: () => import('./views/DashboardView.vue') },
    { path: '/attempts', name: 'attempts', component: () => import('./views/AttemptsView.vue') },
    { path: '/torrents', name: 'torrents', component: () => import('./views/TorrentsView.vue') },
    { path: '/sites', name: 'sites', component: () => import('./views/SitesView.vue') },
    { path: '/settings', name: 'settings', component: () => import('./views/SettingsView.vue') },
    { path: '/:pathMatch(.*)*', redirect: '/' },
  ],
})

createApp(App).use(router).mount('#app')
