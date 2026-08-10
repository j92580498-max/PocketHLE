(function () {
  const input = document.querySelector("#game-search");
  const grid = document.querySelector("#game-grid");
  const empty = document.querySelector("#empty-search");
  if (!input || !grid || !empty) return;
  const cards = Array.from(grid.querySelectorAll(".game-card"));
  input.addEventListener("input", function () {
    const query = input.value.trim().toLowerCase();
    let visible = 0;
    cards.forEach(function (card) {
      const matches = card.textContent.toLowerCase().includes(query);
      card.hidden = !matches;
      if (matches) visible += 1;
    });
    empty.hidden = visible !== 0;
  });
})();
