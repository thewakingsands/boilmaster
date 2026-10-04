const csrf = document.querySelector('meta[name="csrf-token"]').content;
const notice = document.getElementById('notice');
let wasRunning = false;
let submitting = false;
document.querySelectorAll('button[data-action]').forEach((button) => {
  button.addEventListener('click', async () => {
    if (submitting) return;
    submitting = true;
    button.disabled = true;
    notice.textContent = '正在提交操作…';
    try {
      const response = await fetch(button.dataset.action, {
        method: 'POST', headers: { 'X-CSRF-Token': csrf }, credentials: 'same-origin'
      });
      if (!response.ok) throw new Error(await response.text());
      notice.textContent = '操作已接受，正在检查状态。';
      wasRunning = true;
      await refresh();
    } catch (error) {
      notice.textContent = `操作失败：${error.message}`;
      button.disabled = false;
    } finally { submitting = false; }
  });
});
function describe(status) {
  if (status.running || status.state === 'running') return '进行中';
  if (status.error) return `失败：${status.error}`;
  const changed = status.changed ?? status.updated;
  if (changed === true) return '已更新';
  if (changed === false) return '已检查，无变化';
  return status.state === 'error' ? '失败' : '就绪';
}
async function refresh() {
  const response = await fetch('/admin/status', { cache: 'no-store', credentials: 'same-origin' });
  if (!response.ok) throw new Error(`状态查询失败 (${response.status})`);
  const state = await response.json();
  document.getElementById('data-status').textContent = describe(state.data.update);
  document.getElementById('assets-status').textContent = describe(state.assets.reload);
  document.getElementById('docs-status').textContent = describe(state.docs);
  const running = state.data.update.state === 'running' || state.assets.reload.running || state.docs.running;
  if (wasRunning && !running) { location.reload(); return; }
  wasRunning = running;
}
refresh().catch((error) => { notice.textContent = error.message; });
setInterval(() => refresh().catch((error) => { notice.textContent = error.message; }), 3000);
