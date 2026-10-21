import { createClient } from "@connectrpc/connect"
import { createGrpcWebTransport } from "@connectrpc/connect-web"

import { Parrot } from "@/gen/parrot_pb"
import { ParrotWeb, type DescribeHostResponse } from "@/gen/web_pb"

const transport = createGrpcWebTransport({
  baseUrl: location.origin,
  useBinaryFormat: true,
})

export const parrot = createClient(Parrot, transport)
export const parrotWeb = createClient(ParrotWeb, transport)

let hostDescription: Promise<DescribeHostResponse> | undefined

export function describeHost(): Promise<DescribeHostResponse> {
  hostDescription ??= parrotWeb.describeHost({})
  return hostDescription
}
