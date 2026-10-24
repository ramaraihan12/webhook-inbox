// Pages refresh themselves with htmx polling. Skip the polls while the tab is
// hidden, so a forgotten tab does not keep querying the database.
document.addEventListener("htmx:beforeRequest", function (event) {
  if (document.hidden && event.target.hasAttribute("data-poll")) {
    event.preventDefault();
  }
});
