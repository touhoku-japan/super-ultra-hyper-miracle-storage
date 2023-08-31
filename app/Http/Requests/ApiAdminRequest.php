<?php

namespace App\Http\Requests;

use Illuminate\Foundation\Http\FormRequest;
use Illuminate\Http\Exceptions\HttpResponseException;
use Illuminate\Contracts\Validation\Validator;

class ApiAdminRequest extends FormRequest
{
    /**
     * Determine if the user is authorized to make this request.
     *
     * @return bool
     */
    public function authorize()
    {
        return true;
    }

    /**
     * Get the validation rules that apply to the request.
     *
     * @return array<string, mixed>
     */
    public function rules()
    {
        return [
            'password'=>'required|api_admin'
            //
        ];
    }

    protected function failedValidation(Validator $validator)
    {
        $res = response()->json([
            'result' => false,
            'validation' => $validator->errors(),
            'message'=>"Validation Error"
        ],400);
        throw new HttpResponseException($res);
    }

}
