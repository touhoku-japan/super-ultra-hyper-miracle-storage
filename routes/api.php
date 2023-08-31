<?php

use App\Http\Controllers\ApiController;
use Illuminate\Http\Request;
use Illuminate\Support\Facades\Route;

/*
|--------------------------------------------------------------------------
| API Routes
|--------------------------------------------------------------------------
|
| Here is where you can register API routes for your application. These
| routes are loaded by the RouteServiceProvider within a group which
| is assigned the "api" middleware group. Enjoy building your API!
|
*/

/*
Route::middleware('auth:sanctum')->get('/user', function (Request $request) {
    return $request->user();
});
*/


Route::post('/list', [ApiController::class, 'list'])->name('list');
Route::post('/list/download', [ApiController::class, 'list_download'])->name('list.download');
Route::post('/list/deleted/download', [ApiController::class, 'list_deleted_download'])->name('list.deleted.download');

Route::post('/upload', [ApiController::class, 'upload'])->name('upload');
Route::post('/download/{md5}', [ApiController::class, 'download'])->name('download');
//Route::post('/download/md5', [ApiController::class, 'download_md5'])->name('download');

Route::post('/find/md5', [ApiController::class, 'find_md5'])->name('md5');
