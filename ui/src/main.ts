import { createApp } from 'vue';
import App from './App.vue';
import { adoptToken } from './api/client';
import './styles/tokens.css';
import './styles/base.css';

// **口令先落地，再挂载。** 挂载会立刻跑 `App.vue` 的 `onMounted`，那里已经有两
// 批开场请求（运行状态快照 + bootstrap/local）；口令但凡晚一步，最先出门的那
// 一发就是 401，界面直接进「口令失效」终态。放在这里还有第二个作用：地址栏里
// 的 `?token=` 在页面出来的那一刻就被抹掉，而不是等第一个请求。
adoptToken();

createApp(App).mount('#app');
