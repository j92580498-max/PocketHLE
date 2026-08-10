(function () {
  const input = document.querySelector("#game-search");
  if (!input) return;
  const table = input.closest("table");
  const rows = Array.from(table.querySelectorAll("tbody tr"));
  input.addEventListener("input", function () {
    const query = input.value.trim().toLowerCase();
    rows.forEach(function (row) {
      row.hidden = !row.textContent.toLowerCase().includes(query);
    });
  });
})();
