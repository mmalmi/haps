let noticeTimer;
for (const button of document.querySelectorAll('[data-copy]')) {
  button.addEventListener('click', async () => {
    const text = document.getElementById(button.dataset.copy).textContent.trim();
    const notice = document.getElementById('notice');
    try {
      await navigator.clipboard.writeText(text);
      notice.textContent = 'Copied to clipboard';
    } catch {
      notice.textContent = 'Select the command to copy it';
    }
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => { notice.textContent = ''; }, 2400);
  });
}
