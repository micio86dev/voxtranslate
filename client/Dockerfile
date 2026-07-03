# ---- Build stage ----
FROM node:22-slim AS builder
WORKDIR /app

COPY package.json package-lock.json* ./
RUN npm install

COPY . .
# PUBLIC_WS_HOST is inlined at build time (Astro/Vite static build).
ARG PUBLIC_WS_HOST=localhost:3001
ENV PUBLIC_WS_HOST=$PUBLIC_WS_HOST
RUN npm run build

# ---- Runtime stage ----
FROM node:22-slim
WORKDIR /app
RUN npm install -g serve
COPY --from=builder /app/dist ./dist

# Drop root: the node image ships an unprivileged `node` user; serving static files
# needs no root.
USER node

EXPOSE 4321
CMD ["serve", "-s", "dist", "-l", "4321"]
