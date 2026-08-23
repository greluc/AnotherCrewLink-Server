#################################################
# Common base image
#################################################
FROM node:22-alpine AS common
RUN mkdir /app && chown node:node /app
WORKDIR /app
USER node

# Cache dependency installation as it changes less often than source.
COPY --chown=node:node package.json package-lock.json tsconfig.json ./
RUN npm ci --omit=dev && npm cache clean --force

#################################################
# Compile stage
#################################################
FROM common AS build
RUN npm ci
COPY --chown=node:node src/ src/
RUN npm run build

#################################################
# Production stage
#################################################
FROM common
COPY --chown=node:node views/ views/
COPY --chown=node:node public/ public/
COPY --chown=node:node --from=build /app/dist/ dist
EXPOSE 9736
CMD ["node", "dist/index.js"]
