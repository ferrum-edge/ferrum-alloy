/*
 * Starts Swagger UI on Ferrum Alloy's documentation page. The page's
 * Content-Security-Policy forbids inline scripts, so this is a file of its
 * own. It loads the OpenAPI document from the listener that served the page,
 * never from another origin, and never reads configuration from the URL.
 */
(function () {
  "use strict";
  var root = document.getElementById("swagger-ui");
  var documentUrl = new URL(root.getAttribute("data-document"), window.location.href);
  if (documentUrl.origin !== window.location.origin) {
    root.textContent = "The OpenAPI document must be served from this origin.";
    return;
  }
  window.SwaggerUIBundle({
    url: documentUrl.href,
    dom_id: "#swagger-ui",
    layout: "BaseLayout",
    deepLinking: false,
    queryConfigEnabled: false,
    validatorUrl: null,
    // Documentation only: "Try it out" would send requests to the API.
    supportedSubmitMethods: [],
    tryItOutEnabled: false
  });
})();
