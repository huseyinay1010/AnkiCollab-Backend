# Multi-Stage Build: Starte mit einem neueren Rust-Image
FROM rust:1.85 AS builder

# Kopiere die Cargo-Manifeste, um die Abhängigkeiten zu cachen
WORKDIR /app
COPY Cargo.toml Cargo.lock ./

# Installiere die Abhängigkeiten
RUN cargo build --release

# Kopiere den Rest des Codes
COPY . .

# Erstelle den Release-Build
RUN cargo build --release

# Finales, schlankes Image erstellen
FROM debian:stable-slim
# Lege hier den User oder die Umgebungsvariablen fest
WORKDIR /app

# Kopiere die fertige Binärdatei aus dem Builder-Image
COPY --from=builder /app/target/release/anki-collab-backend /usr/local/bin/anki-collab-backend

# Befehl zum Starten der Anwendung
CMD ["/usr/local/bin/anki-collab-backend"]
