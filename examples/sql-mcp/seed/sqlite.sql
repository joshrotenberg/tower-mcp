-- Seed data for sql-mcp SQLite testing

CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    email TEXT UNIQUE NOT NULL,
    role TEXT NOT NULL DEFAULT 'member',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS products (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    description TEXT,
    price REAL NOT NULL,
    category TEXT NOT NULL,
    in_stock INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    product_id INTEGER NOT NULL REFERENCES products(id),
    quantity INTEGER NOT NULL DEFAULT 1,
    total REAL NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    ordered_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_orders_user ON orders(user_id);
CREATE INDEX IF NOT EXISTS idx_orders_product ON orders(product_id);
CREATE INDEX IF NOT EXISTS idx_orders_status ON orders(status);
CREATE INDEX IF NOT EXISTS idx_products_category ON products(category);

INSERT OR IGNORE INTO users (id, name, email, role) VALUES
    (1, 'Alice Johnson', 'alice@example.com', 'admin'),
    (2, 'Bob Smith', 'bob@example.com', 'member'),
    (3, 'Charlie Brown', 'charlie@example.com', 'member'),
    (4, 'Diana Prince', 'diana@example.com', 'moderator'),
    (5, 'Eve Wilson', 'eve@example.com', 'member');

INSERT OR IGNORE INTO products (id, name, description, price, category) VALUES
    (1, 'Widget A', 'Standard widget', 9.99, 'widgets'),
    (2, 'Widget B', 'Premium widget with extras', 19.99, 'widgets'),
    (3, 'Gadget X', 'Entry-level gadget', 29.99, 'gadgets'),
    (4, 'Gadget Y', 'Professional gadget', 49.99, 'gadgets'),
    (5, 'Doohickey', 'Multi-purpose doohickey', 14.99, 'accessories'),
    (6, 'Thingamajig', 'Essential thingamajig', 7.99, 'accessories');

INSERT OR IGNORE INTO orders (id, user_id, product_id, quantity, total, status) VALUES
    (1, 1, 1, 2, 19.98, 'completed'),
    (2, 1, 3, 1, 29.99, 'completed'),
    (3, 2, 2, 1, 19.99, 'shipped'),
    (4, 3, 4, 1, 49.99, 'pending'),
    (5, 3, 5, 3, 44.97, 'pending'),
    (6, 4, 1, 5, 49.95, 'completed'),
    (7, 5, 6, 2, 15.98, 'cancelled');
