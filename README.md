# Nestjs OpenAPI generator

Goal: Deterministic automatic generation of OpenAPI / AsyncAPI files based on the codebase that will be put in a CI/CD pipeline

## Existing solutions - not matching our need

- https://github.com/BeerMoneyDev/nest-openapi-tools
- https://github.com/Eywek/typoa
- https://github.com/lukeautry/tsoa
- https://github.com/flamewow/nestjs-asyncapi
- https://github.com/ts-oas/ts-oas
- https://github.com/samchon/nestia => @nestia/sdk: requires a specific library to be installed and has issues with project config not using nestia, doesn't allow to add plugins to determine decorator => openapi types.
- @nestjs/swagger: requires specific decorators in the code, often redundant with the rest of the codebase and adds a library not used in production.

Except nestia, all those tools don't interpret the code (and jsdoc) statically to generate the openapi

## Target

- NestJS apps (first!)
- TypeScript code in general
- JavaScript code in general

## Goals

Same logic/behavior than prettier (https://prettier.io/), eslint (https://eslint.org/) or oxlint (https://oxc.rs/docs/guide/usage/linter) tools:

- config file at the root of codebase
- cli with only the bare minimum options
- extension possible:
  - to implement new specific behavior, i.e. new JSDoc attributes definition or new decorator behavior
  - to override current specific behavior
- speed, should be in rust

## Definition

Base for the OpenAPI / AsyncAPI definitions:

- jsdoc
- decorators
- class
- interface
