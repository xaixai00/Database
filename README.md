# MotoShop — local motorcycle parts store (Rust)

Two Windows executables that share one local SQLite database:

| App | File | Opens | Who uses it |
|---|---|---|---|
| **Admin** | `shop-admin.exe` | http://127.0.0.1:8081 | The shop owner — add products, choose what's featured on the Home screen, store settings, see customers |
| **Customer** | `shop-customer.exe` | http://127.0.0.1:8080 | Customers — sign up / sign in, browse, product popup, cart |

Each `.exe` is a small web server with its UI built in. Double-click it, a console window shows the address, and your browser opens automatically. Close the console window (or press Ctrl+C) to stop it.

Both apps read and write **`shop.db` in the same folder as the .exe files**, so keep them together. Nothing needs to be installed — the database engine is compiled into the executables.

## Features

**Customer app**
- Sign-in screen with *Sign in* / *Create account*. Accounts are stored in the database (passwords hashed with Argon2id), sessions last 30 days.
- **Home** tab — the products the admin featured, in the admin's order.
- **Products** tab — every product, with search (name, description, category, motorcycle model) and category filters.
- Clicking a product opens a popup with:
  1. Main item (image, category, name)
  2. Description
  3. Sell price (updates with the chosen variation)
  4. Variations (each can have its own price)
  5. Features
  6. Available for (compatible models)
  7. Quantity + **Add to cart**
- **Cart** tab — every line with its variation, unit price and line total; **+ / −** buttons or type a quantity, remove a line, empty the cart, and *Add more items*. The cart is saved per customer in the database.

**Admin app**
- **Products** — add, edit and delete products: name, category, description, base price, main image (upload or drag-and-drop; large photos are resized), variations with optional price overrides, features, and compatible models. *Load sample products* fills an empty catalog for testing.
- **Home screen** — feature existing products on the customer Home tab, reorder them (↑ ↓) or remove them. Changes are live immediately.
- **Store settings** — shop name, tagline and currency symbol (default ₱) shown in both apps.
- **Customers** — who signed up and how many items they have in their cart.

## Building with GitHub Actions

The workflow is in `.github/workflows/build.yml`.

1. Push this folder to a GitHub repository (commit `Cargo.lock` too — the build uses `--locked`).
2. Every push to `main`/`master`, every pull request, or a manual run (*Actions → Build → Run workflow*) will:
   - run `cargo fmt --check`, `cargo clippy` and the unit tests on Linux, then
   - build `shop-admin.exe` and `shop-customer.exe` on Windows.
3. Download them from the run page under **Artifacts → MotoShop-windows-x64**.
4. To publish a release, push a tag that starts with `v`:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
   The workflow attaches `MotoShop-windows-x64-v0.1.0.zip` to a new GitHub Release.

## Building locally

Requires Rust (https://rustup.rs). On Windows, the MSVC build tools that rustup asks for are enough.

```bash
cargo build --release
# binaries: target/release/shop-admin(.exe) and target/release/shop-customer(.exe)
cargo test        # unit tests
```

During development you can run both with `cargo run -p shop-admin` and `cargo run -p shop-customer` in two terminals; they share `target/debug/shop.db`.

## Configuration (optional environment variables)

| Variable | Default | Purpose |
|---|---|---|
| `SHOP_DB` | `shop.db` next to the .exe | Use a different database file |
| `CUSTOMER_ADDR` | `127.0.0.1:8080` | Customer app address. Set `0.0.0.0:8080` to let phones/PCs on your Wi-Fi open the store at `http://<this-PC's-IP>:8080` (allow it through Windows Firewall when asked) |
| `ADMIN_ADDR` | `127.0.0.1:8081` | Admin app address. The admin app only accepts requests addressed to this computer |
| `STORE_URL` | derived from `CUSTOMER_ADDR` | Where the admin's *Open store* link points |
| `NO_BROWSER` | unset | Set to anything to stop the apps opening a browser on start |

Example (Command Prompt):
```bat
set CUSTOMER_ADDR=0.0.0.0:8080
shop-customer.exe
```

## Notes

- **Backups:** copy `shop.db` (and `shop.db-wal` / `shop.db-shm` if present) while the apps are closed.
- **Admin access:** the admin app has no password; it's protected by only listening on this computer. Don't change `ADMIN_ADDR` to `0.0.0.0` unless you add a login.
- **Fonts:** the UI uses Google Fonts when online and falls back to Windows fonts offline.
- **Windows SmartScreen** may warn the first time you run an unsigned .exe — choose *More info → Run anyway*.
- Checkout/orders aren't part of this version; the cart is where the flow ends.

## Project layout

```
Cargo.toml                     workspace
crates/shop-core/              shared: SQLite schema & queries, auth, web helpers, base.css
crates/shop-admin/             admin.exe  (src/main.rs + assets/admin.html)
crates/shop-customer/          customer.exe (src/main.rs + assets/customer.html)
.github/workflows/build.yml    CI: lint, test, Windows build, release on tags
```
