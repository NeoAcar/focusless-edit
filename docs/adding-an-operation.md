# Adding an editing operation

Every feature follows the same end-to-end path.

1. Add small, serializable parameters to `focusless-core::Operation`. Validate
   parameter ranges in the core.
2. Add the corresponding `Command` behavior so the user can undo the change.
   Group continuous slider movement into one command.
3. Apply the operation in the renderer's documented canonical order inside
   `focusless-engine-vips::apply_operations`. Preserve alpha, geometry, and
   color-space behavior explicitly.
4. Add presentation and callbacks to the Slint UI without calling libvips.
5. Connect the controller callback to the document change, preview generation,
   and autosave flow.
6. Test:
   - Parameter boundaries
   - Undo/redo
   - JSON round-trip
   - Known pixel output
   - Alpha preservation
   - Stale preview rejection

Do not change the meaning of an existing schema. If an old project produces a
different image under the new code, increment the schema version and add an
explicit migration.

Geometry operations require additional coverage for output dimensions,
normalized-coordinate boundaries, operation ordering, and interactions with
existing crops or rotations.
