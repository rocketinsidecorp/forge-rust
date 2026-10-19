// Loaded by the layout as a module. Bare specifiers resolve through the
// import map pinned in src/adapters/http/views/mod.rs (Site::load).
import "@hotwired/turbo";
import { Application } from "@hotwired/stimulus";

const application = Application.start();

// Register Stimulus controllers here. Pin each module first, for example
// `.pin("controllers/hello", "controllers/hello_controller.js")`, then:
//   import HelloController from "controllers/hello";
//   application.register("hello", HelloController);
window.Stimulus = application;
