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
    { path: '/seeds', name: 'seeds', component: () => import('./views/SeedsView.vue') },
    // 旧地址保留：`/attempts?state=verify_failed` 这类书签和外部链接还在用，
    // 直接把旧的状态参数映射成新的组过滤条件，而不是让人 404 或者退回首页。
    {
      path: '/attempts',
      redirect: (to) => {
        const state = String(to.query.state ?? '')
        const filter =
          state === 'verify_failed' || state === 'missing_files'
            ? 'has_failure'
            : state === 'added' || state === 'verifying'
              ? 'verifying'
              : 'all'
        return { path: '/seeds', query: filter === 'all' ? {} : { filter } }
      },
    },
    { path: '/torrents', name: 'torrents', component: () => import('./views/TorrentsView.vue') },
    { path: '/sites', name: 'sites', component: () => import('./views/SitesView.vue') },
    { path: '/settings', name: 'settings', component: () => import('./views/SettingsView.vue') },
    { path: '/:pathMatch(.*)*', redirect: '/' },
  ],
})

createApp(App).use(router).mount('#app')
